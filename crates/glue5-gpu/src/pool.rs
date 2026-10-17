//! Compile threads: shader modules and pipelines are built off the command
//! thread (a pipeline compile takes milliseconds; hundreds arrive when a new
//! area streams in). Urgent jobs (something is waiting) run before
//! background ones (objects to free).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Queues {
    urgent: VecDeque<Job>,
    background: VecDeque<Job>,
}

pub struct Pool {
    queues: Arc<(Mutex<Queues>, Condvar)>,
}

impl Pool {
    pub fn new(name: &str, threads: usize) -> Pool {
        let queues: Arc<(Mutex<Queues>, Condvar)> = Arc::default();
        for i in 0..threads {
            let q = queues.clone();
            std::thread::Builder::new()
                .name(format!("{name} {i}"))
                .spawn(move || {
                    loop {
                        let (job, qos) = {
                            let (lock, cv) = &*q;
                            let mut queues = lock.lock().unwrap();
                            loop {
                                if let Some(job) = queues.urgent.pop_front() {
                                    break (job, glue5_runtime::qos::compile_qos());
                                }
                                if let Some(job) = queues.background.pop_front() {
                                    break (job, glue5_runtime::qos::Qos::Utility);
                                }
                                queues = cv.wait(queues).unwrap();
                            }
                        };
                        // Idle module destruction is not latency-critical.
                        // Restore the higher class before the next urgent job.
                        glue5_runtime::qos::set(qos);
                        job();
                    }
                })
                .expect("spawning a compile thread");
        }
        Pool { queues }
    }

    pub fn urgent(&self, job: impl FnOnce() + Send + 'static) {
        let (lock, cv) = &*self.queues;
        lock.lock().unwrap().urgent.push_back(Box::new(job));
        cv.notify_one();
    }

    pub fn background(&self, job: impl FnOnce() + Send + 'static) {
        let (lock, cv) = &*self.queues;
        lock.lock().unwrap().background.push_back(Box::new(job));
        cv.notify_one();
    }
}
