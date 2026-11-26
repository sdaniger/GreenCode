//! `/attach` and drag-and-drop: any file becomes a path the agent can read.
use super::*;
use std::path::Path;

/// T3's limits: files per message and bytes per file.
const MAX_FILES: usize = 100;
const MAX_BYTES: u64 = 50 * 1024 * 1024;
const IMAGES: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Plan {
    /// Inside the project: mentioned as `@rel`.
    Mention(String),
    /// Outside the project: copied under `~/.greencode/attachments`.
    Copy,
    TooLarge,
    /// Sent as an image when the prompt is submitted; the path is inserted unchanged.
    Image,
}

pub(super) fn attach_plan(path: &Path, project: &Path, size: u64) -> Plan {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if ext.as_deref().is_some_and(|e| IMAGES.contains(&e)) {
        return Plan::Image;
    }
    if size > MAX_BYTES {
        return Plan::TooLarge;
    }
    match path.strip_prefix(project) {
        Ok(rel) if !rel.as_os_str().is_empty() => Plan::Mention(rel.to_string_lossy().into_owned()),
        _ => Plan::Copy,
    }
}

fn human(size: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let (mut v, mut u) = (size as f64, 0);
    while v >= 1024. && u < UNITS.len() - 1 {
        v /= 1024.;
        u += 1;
    }
    if u == 0 {
        format!("{size} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

impl Workspace {
    /// Adds an image to the composer's previews; it is sent with the next prompt.
    pub(super) fn attach_image(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // The prompt carries images as whitespace-separated paths, so a spaced name is copied first.
        let path = if path.to_string_lossy().contains(char::is_whitespace) {
            let name = path.file_name().map(|n| n.to_string_lossy().replace(char::is_whitespace, "-"));
            let dir = greencode_core::home().join("attachments");
            let copy = dir.join(format!("{}-{}", now(), name.unwrap_or_default()));
            if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::copy(&path, &copy)) {
                return self.fail(format!("画像を添付できません: {e}"));
            }
            copy
        } else {
            path
        };
        let images = &mut self.threads[self.active].images;
        if !images.contains(&path) {
            images.push(path);
        }
        cx.notify();
    }

    /// Mentions each path in the composer, copying files from outside the project.
    pub(super) fn attach_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.len() > MAX_FILES {
            self.fail(format!("一度に添付できるのは {MAX_FILES} ファイルまでです"));
        }
        let project = self.thread().project();
        let project = project.canonicalize().unwrap_or(project);
        for path in paths.into_iter().take(MAX_FILES) {
            let path = path.canonicalize().unwrap_or(path);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let Ok(meta) = std::fs::metadata(&path) else {
                self.fail(format!("{name} を読み込めません"));
                continue;
            };
            if meta.is_dir() {
                let mention = match path.strip_prefix(&project) {
                    Ok(rel) if !rel.as_os_str().is_empty() => {
                        PathBuf::from(format!("@{}", rel.display()))
                    }
                    _ => path.clone(),
                };
                self.insert_path(&mention, window, cx);
                self.notice(format!("添付: {name}（フォルダ）"));
                continue;
            }
            let size = meta.len();
            match attach_plan(&path, &project, size) {
                Plan::TooLarge => self.fail(format!("{name} は 50 MiB を超えています")),
                Plan::Image => {
                    self.attach_image(path.clone(), cx);
                    self.notice(format!("添付: {name}（{}）", human(size)));
                }
                Plan::Mention(rel) => {
                    self.insert_path(Path::new(&format!("@{rel}")), window, cx);
                    self.notice(format!("添付: {name}（{}）", human(size)));
                }
                Plan::Copy => {
                    let dir = greencode_core::home().join("attachments");
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_nanos());
                    let dest = dir.join(format!("{now}-{name}"));
                    match std::fs::create_dir_all(&dir).and_then(|_| std::fs::copy(&path, &dest)) {
                        Ok(_) => {
                            self.insert_path(&dest, window, cx);
                            self.notice(format!("添付: {name}（{}）", human(size)));
                        }
                        Err(e) => self.fail(format!("{name} を添付できません: {e}")),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{attach_plan, Plan, MAX_BYTES};
    use crate::workspace::tests::open;
    use gpui_kit::TestAppContext;
    use std::path::Path;

    #[test]
    fn plan_decides_by_kind_size_and_location() {
        let project = Path::new("/p");
        assert_eq!(
            attach_plan(Path::new("/p/src/a.rs"), project, 10),
            Plan::Mention("src/a.rs".into())
        );
        assert_eq!(attach_plan(Path::new("/o/a.rs"), project, 10), Plan::Copy);
        assert_eq!(attach_plan(Path::new("/o/a.mp4"), project, 10), Plan::Copy);
        assert_eq!(
            attach_plan(Path::new("/p/a.bin"), project, MAX_BYTES + 1),
            Plan::TooLarge
        );
        assert_eq!(
            attach_plan(Path::new("/p/a.bin"), project, MAX_BYTES),
            Plan::Mention("a.bin".into())
        );
        assert_eq!(
            attach_plan(Path::new("/o/A.PNG"), project, MAX_BYTES + 1),
            Plan::Image
        );
        assert_eq!(attach_plan(Path::new("/pp/a.rs"), project, 1), Plan::Copy);
    }

    #[gpui_kit::test]
    async fn attaching_copies_outside_files_and_mentions_project_files(cx: &mut TestAppContext) {
        let (ws, cx) = open(cx);
        let root = std::env::temp_dir().join(format!("greencode-attach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let outside = root.join("note.txt");
        std::fs::write(&outside, "hello").unwrap();
        let project = ws.read_with(cx, |ws, _| ws.thread().project());
        let inside = project.join("greencode-attach-test.txt");
        std::fs::write(&inside, "x").unwrap();
        cx.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.attach_paths(vec![outside, inside.clone()], window, cx)
            })
        });
        let value = ws.read_with(cx, |ws, cx| ws.input.read(cx).value().to_string());
        let _ = std::fs::remove_file(&inside);
        let _ = std::fs::remove_dir_all(&root);
        let copied = greencode_core::home().join("attachments");
        assert!(value.contains(&copied.display().to_string()), "{value}");
        assert!(value.contains("-note.txt"), "{value}");
        assert!(value.contains("@greencode-attach-test.txt"), "{value}");
        let entries = std::fs::read_dir(&copied).unwrap().count();
        assert_eq!(entries, 1);
        let _ = std::fs::remove_dir_all(greencode_core::home());
    }
}
