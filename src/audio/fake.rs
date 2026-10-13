//! A PipeWire graph in memory, for the tests: it records what Navette did to it.

use std::io;
use std::sync::{Arc, Mutex};

use super::jitter::Jitter;
use super::{DefaultSink, Graph, OnSound};

#[derive(Default)]
pub struct State {
    pub default: Option<DefaultSink>,
    pub sinks: Vec<(String, Arc<Mutex<OnSound>>)>,
    pub playbacks: Vec<(String, Arc<Mutex<Jitter>>)>,
    /// What was done, in order.
    pub log: Vec<String>,
    /// Changes of the default fail, as when PipeWire is gone.
    pub refuse_defaults: bool,
    /// How long a change of the default takes.
    pub slow_defaults: std::time::Duration,
}

#[derive(Clone, Default)]
pub struct FakeGraph(pub Arc<Mutex<State>>);

impl FakeGraph {
    pub fn with_default(name: &str) -> Self {
        let graph = Self::default();
        graph.state().default = Some(DefaultSink::named(name));
        graph
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }
}

pub struct FakeNode {
    graph: FakeGraph,
    name: String,
}

impl Drop for FakeNode {
    fn drop(&mut self) {
        let mut state = self.graph.state();
        state.sinks.retain(|(name, _)| *name != self.name);
        state.playbacks.retain(|(name, _)| *name != self.name);
        state.log.push(format!("remove {}", self.name));
    }
}

impl Graph for FakeGraph {
    type Node = FakeNode;

    fn default_sink(&self) -> io::Result<Option<DefaultSink>> {
        Ok(self.state().default.clone())
    }

    fn set_default_sink(&self, sink: Option<&DefaultSink>) -> io::Result<()> {
        let slow = self.state().slow_defaults;
        std::thread::sleep(slow);
        let mut state = self.state();
        if state.refuse_defaults {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PipeWire is gone",
            ));
        }
        state.default = sink.cloned();
        state.log.push(match sink {
            Some(sink) => format!("default {}", sink.0),
            None => "default cleared".to_owned(),
        });
        Ok(())
    }

    fn open_sink(&self, name: &str, _description: &str, on_sound: OnSound) -> io::Result<FakeNode> {
        let mut state = self.state();
        state
            .sinks
            .push((name.to_owned(), Arc::new(Mutex::new(on_sound))));
        state.log.push(format!("sink {name}"));
        Ok(FakeNode {
            graph: self.clone(),
            name: name.to_owned(),
        })
    }

    fn open_playback(
        &self,
        name: &str,
        _description: &str,
        jitter: Arc<Mutex<Jitter>>,
    ) -> io::Result<FakeNode> {
        let mut state = self.state();
        state.playbacks.push((name.to_owned(), jitter));
        state.log.push(format!("playback {name}"));
        Ok(FakeNode {
            graph: self.clone(),
            name: name.to_owned(),
        })
    }
}
