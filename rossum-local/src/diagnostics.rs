use std::collections::VecDeque;
use std::sync::Mutex;

const CAP: usize = 100;

pub struct DiagLog {
    entries: Mutex<VecDeque<String>>,
}

impl Default for DiagLog {
    fn default() -> Self {
        Self { entries: Mutex::new(VecDeque::with_capacity(CAP)) }
    }
}

impl DiagLog {
    pub fn push(&self, line: impl Into<String>) {
        let mut q = self.entries.lock().unwrap();
        if q.len() == CAP {
            q.pop_front();
        }
        q.push_back(line.into());
    }

    #[allow(dead_code)]
    pub fn snapshot(&self) -> Vec<String> {
        let q = self.entries.lock().unwrap();
        q.iter().cloned().collect()
    }
}
