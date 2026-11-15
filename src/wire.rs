use lumagame_core::ipc::{self, Event, FromEngine};
use std::{io::{self, Write}, sync::{Arc, Mutex}};

#[derive(Clone)]
pub struct Output(Arc<Mutex<io::Stdout>>);
impl Output {
    pub fn new() -> Self { Self(Arc::new(Mutex::new(io::stdout()))) }
    pub fn send(&self, re: Option<u64>, event: Event) {
        let message = FromEngine { re, event };
        let mut output = self.0.lock().unwrap();
        let _ = output.write_all(ipc::encode(&message).as_bytes()).and_then(|_| output.flush());
    }
    pub fn told(&self, event: Event) { self.send(None, event); }
}
