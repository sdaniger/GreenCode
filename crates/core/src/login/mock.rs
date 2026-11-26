//! A scripted local HTTP server for the sign-in tests: answers each request in order and records it.

use crate::localserver::{serve, Request, Response};
use std::{collections::VecDeque, sync::{Arc, Mutex}};

#[derive(Clone, Debug)]
pub(crate) struct Recorded {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The form field of a form-encoded body.
    pub fn field(&self, name: &str) -> Option<String> {
        url::form_urlencoded::parse(self.body.as_bytes()).find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
    }

    /// The JSON body.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_default()
    }
}

pub(crate) struct Mock {
    pub origin: String,
    log: Arc<Mutex<Vec<Recorded>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Mock {
    /// Answers the nth request with the nth `(status, body)`; `{origin}` in a body becomes the server's URL.
    pub async fn start(answers: &[(&str, &str)]) -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let answers: VecDeque<(String, String)> =
            answers.into_iter().map(|(status, body)| (status.to_string(), body.replace("{origin}", &origin))).collect();
        let queue = Arc::new(Mutex::new(answers));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (q, l) = (queue.clone(), log.clone());
        let task = tokio::spawn(async move {
            let _ = serve(listener, 1 << 20, move |req: Request| {
                let (q, l) = (q.clone(), l.clone());
                async move {
                    l.lock().unwrap().push(Recorded {
                        path: req.path,
                        headers: req.headers,
                        body: String::from_utf8_lossy(&req.body).to_string(),
                    });
                    let (status, body) = q.lock().unwrap().pop_front().unwrap_or_else(|| ("500".into(), "{}".into()));
                    let status: &'static str = match status.get(..3).unwrap_or("500") {
                        "200" => "200 OK",
                        "400" => "400 Bad Request",
                        "401" => "401 Unauthorized",
                        _ => "500 Internal Server Error",
                    };
                    Response::new(status, "application/json", body)
                }
            }).await;
        });
        Mock { origin, log, task }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.log.lock().unwrap().clone()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
