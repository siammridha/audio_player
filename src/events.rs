//! Server-sent events: pushes each kind of update to every open page, only
//! when it changed.

use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Default)]
pub struct Events {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The last data sent for each event, in the order first published, so
    /// a page that connects gets the status before the file list.
    latest: Vec<(&'static str, String)>,
    clients: Vec<Sender<String>>,
}

impl Events {
    pub fn publish(&self, name: &'static str, value: &serde_json::Value) {
        let data = value.to_string();
        let mut inner = self.inner.lock().unwrap();
        match inner.latest.iter_mut().find(|(n, _)| *n == name) {
            Some((_, last)) if *last == data => return,
            Some((_, last)) => *last = data.clone(),
            None => inner.latest.push((name, data.clone())),
        }
        let message = format(name, &data);
        inner.clients.retain(|c| c.send(message.clone()).is_ok());
    }

    /// Starts with the latest copy of every event, then gets each new one.
    pub fn subscribe(&self) -> Receiver<String> {
        let (tx, rx) = mpsc::channel();
        let mut inner = self.inner.lock().unwrap();
        for (name, data) in &inner.latest {
            let _ = tx.send(format(name, data));
        }
        inner.clients.push(tx);
        rx
    }
}

fn format(name: &str, data: &str) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_an_event() {
        assert_eq!(format("status", "{}"), "event: status\ndata: {}\n\n");
    }

    #[test]
    fn a_new_subscriber_gets_the_latest_events_first() {
        let events = Events::default();
        events.publish("status", &json!(1));
        events.publish("files", &json!([]));
        events.publish("status", &json!(2));
        let rx = events.subscribe();
        assert_eq!(rx.try_recv().unwrap(), "event: status\ndata: 2\n\n");
        assert_eq!(rx.try_recv().unwrap(), "event: files\ndata: []\n\n");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn the_same_value_is_sent_once() {
        let events = Events::default();
        let rx = events.subscribe();
        events.publish("status", &json!(1));
        events.publish("status", &json!(1));
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_closed_subscriber_is_dropped() {
        let events = Events::default();
        drop(events.subscribe());
        let _open = events.subscribe();
        events.publish("status", &json!(1));
        assert_eq!(events.inner.lock().unwrap().clients.len(), 1);
    }
}
