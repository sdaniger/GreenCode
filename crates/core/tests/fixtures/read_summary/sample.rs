//! Fixture for the read summary tests: one long body, one nested block, two short bodies.
use crate::audit;

/// Runs the pipeline.
pub fn pipeline() {
    let stage_1 = keep(1);
    let stage_2 = keep(2);
    let stage_3 = keep(3);
    let stage_4 = keep(4);
    let stage_5 = keep(5);
    let stage_6 = keep(6);
    let stage_7 = keep(7);
    let stage_8 = keep(8);
    let stage_9 = keep(9);
    let stage_10 = keep(10);
    let stage_11 = keep(11);
    let stage_12 = keep(12);
    let stage_13 = keep(13);
    let stage_14 = keep(14);
}

fn small() {
    one();
}

fn nested(ready: bool) {
    if ready {
        audit(1);
        audit(2);
        audit(3);
        audit(4);
        audit(5);
        audit(6);
    }
    finish();
}

fn trivial_1() {
    done(1);
}

fn trivial_2() {
    done(2);
}

fn trivial_3() {
    done(3);
}

fn trivial_4() {
    done(4);
}

fn trivial_5() {
    done(5);
}

fn report_header() -> &'static str {
    "summary fixture"
}
