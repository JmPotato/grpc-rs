// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{Builder as ThreadBuilder, JoinHandle};

use cpu_observer::{CollectorRegHandle, Guard, ObserverTag, ObserverTagFactory};

use crate::grpc_sys;

use crate::cq::{CompletionQueue, CompletionQueueHandle, EventType, WorkQueue};
use crate::task::CallTag;

thread_local! {
    static OBSERVER_TAG: RefCell<Option<ObserverTag>> = RefCell::new(None);
}

pub(crate) fn attach_new_observer_tag(
    observer_tag_factory: Option<&ObserverTagFactory>,
) -> Option<Guard> {
    OBSERVER_TAG.with(|tag| {
        observer_tag_factory?;
        let new_tag = observer_tag_factory.unwrap().new_tag("request_call");
        let guard = new_tag.attach();
        *tag.borrow_mut() = Some(new_tag);
        Some(guard)
    })
}

fn attach_cur_observer_tag(observer_tag_factory: Option<&ObserverTagFactory>) -> Option<Guard> {
    OBSERVER_TAG.with(|tag| {
        if let Some(cur_tag) = tag.borrow().as_ref() {
            return Some(cur_tag.attach());
        }
        observer_tag_factory?;
        attach_new_observer_tag(observer_tag_factory)
    })
}

pub(crate) fn update_observer_tag(method_path: &[u8]) {
    OBSERVER_TAG.with(|tag| {
        if let Some(cur_tag) = tag.borrow().as_ref() {
            cur_tag.update(String::from_utf8(method_path.to_vec()).unwrap());
        }
    });
}

// event loop
fn poll_queue(tx: mpsc::Sender<CompletionQueue>, observer_tag_factory: Option<ObserverTagFactory>) {
    let cq = Arc::new(CompletionQueueHandle::new());
    let worker_info = Arc::new(WorkQueue::new());
    let cq = CompletionQueue::new(cq, worker_info);
    tx.send(cq.clone()).expect("send back completion queue");
    let observer_tag_factory = observer_tag_factory.as_ref();
    loop {
        {
            // Get and resolve the request tag.
            let _guard = attach_cur_observer_tag(observer_tag_factory);
            let e = cq.next();
            match e.type_ {
                EventType::GRPC_QUEUE_SHUTDOWN => break,
                // timeout should not happen in theory.
                EventType::GRPC_QUEUE_TIMEOUT => continue,
                EventType::GRPC_OP_COMPLETE => {}
            }

            let tag: Box<CallTag> = unsafe { Box::from_raw(e.tag as _) };
            tag.resolve(&cq, e.success != 0);
        }

        // Finish the unfinished work.
        while let Some(work) = unsafe { cq.worker.pop_work() } {
            work.finish(observer_tag_factory);
        }
    }
}

/// [`Environment`] factory in order to configure the properties.
pub struct EnvBuilder {
    cq_count: usize,
    name_prefix: Option<String>,
    after_start: Option<Arc<dyn Fn() + Send + Sync>>,
    before_stop: Option<Arc<dyn Fn() + Send + Sync>>,
    observer_tag_factory: Option<ObserverTagFactory>,
    cpu_collector_reg_handle: Option<CollectorRegHandle>,
}

impl EnvBuilder {
    /// Initialize a new [`EnvBuilder`].
    pub fn new() -> EnvBuilder {
        EnvBuilder {
            cq_count: unsafe { grpc_sys::gpr_cpu_num_cores() as usize },
            name_prefix: None,
            after_start: None,
            before_stop: None,
            observer_tag_factory: None,
            cpu_collector_reg_handle: None,
        }
    }

    /// Set the number of completion queues and polling threads. Each thread polls
    /// one completion queue.
    ///
    /// # Panics
    ///
    /// This method will panic if `count` is 0.
    pub fn cq_count(mut self, count: usize) -> EnvBuilder {
        assert!(count > 0);
        self.cq_count = count;
        self
    }

    /// Set the thread name prefix of each polling thread.
    pub fn name_prefix<S: Into<String>>(mut self, prefix: S) -> EnvBuilder {
        self.name_prefix = Some(prefix.into());
        self
    }

    /// Execute function `f` after each thread is started but before it starts doing work.
    pub fn after_start<F: Fn() + Send + Sync + 'static>(mut self, f: F) -> EnvBuilder {
        self.after_start = Some(Arc::new(f));
        self
    }

    /// Execute function `f` before each thread stops.
    pub fn before_stop<F: Fn() + Send + Sync + 'static>(mut self, f: F) -> EnvBuilder {
        self.before_stop = Some(Arc::new(f));
        self
    }

    /// Init the CPU usage observer for the gRPC polling thread.
    pub fn init_cpu_observer(
        mut self,
        observer_tag_factory: ObserverTagFactory,
        cpu_collector_reg_handle: CollectorRegHandle,
    ) -> EnvBuilder {
        self.observer_tag_factory = Some(observer_tag_factory);
        self.cpu_collector_reg_handle = Some(cpu_collector_reg_handle);
        self
    }

    /// Finalize the [`EnvBuilder`], build the [`Environment`] and initialize the gRPC library.
    pub fn build(self) -> Environment {
        unsafe {
            grpc_sys::grpc_init();
        }
        let mut cqs = Vec::with_capacity(self.cq_count);
        let mut handles = Vec::with_capacity(self.cq_count);
        let (tx, rx) = mpsc::channel();
        for i in 0..self.cq_count {
            let tx_i = tx.clone();
            let mut builder = ThreadBuilder::new();
            if let Some(ref prefix) = self.name_prefix {
                builder = builder.name(format!("{}-{}", prefix, i));
            }
            let after_start = self.after_start.clone();
            let before_stop = self.before_stop.clone();
            let observer_tag_factory = self.observer_tag_factory.clone();
            let handle = builder
                .spawn(move || {
                    if let Some(f) = after_start {
                        f();
                    }
                    poll_queue(tx_i, observer_tag_factory);
                    if let Some(f) = before_stop {
                        f();
                    }
                })
                .unwrap();
            handles.push(handle);
        }
        for _ in 0..self.cq_count {
            cqs.push(rx.recv().unwrap());
        }

        Environment {
            cqs,
            idx: AtomicUsize::new(0),
            _handles: handles,
            observer_tag_factory: self.observer_tag_factory,
            cpu_collector_reg_handle: self.cpu_collector_reg_handle,
        }
    }
}

/// An object that used to control concurrency and start gRPC event loop.
pub struct Environment {
    cqs: Vec<CompletionQueue>,
    idx: AtomicUsize,
    _handles: Vec<JoinHandle<()>>,
    observer_tag_factory: Option<ObserverTagFactory>,
    cpu_collector_reg_handle: Option<CollectorRegHandle>,
}

impl Environment {
    /// Initialize gRPC and create a thread pool to poll completion queue. The thread pool size
    /// and the number of completion queue is specified by `cq_count`. Each thread polls one
    /// completion queue.
    ///
    /// # Panics
    ///
    /// This method will panic if `cq_count` is 0.
    pub fn new(cq_count: usize) -> Environment {
        assert!(cq_count > 0);
        EnvBuilder::new()
            .name_prefix("grpc-poll")
            .cq_count(cq_count)
            .build()
    }

    /// Get all the created completion queues.
    pub fn completion_queues(&self) -> &[CompletionQueue] {
        self.cqs.as_slice()
    }

    /// Pick an arbitrary completion queue.
    pub fn pick_cq(&self) -> CompletionQueue {
        let idx = self.idx.fetch_add(1, Ordering::Relaxed);
        self.cqs[idx % self.cqs.len()].clone()
    }

    pub(crate) fn observer_tag_factory(&self) -> Option<ObserverTagFactory> {
        self.observer_tag_factory.clone()
    }

    pub fn cpu_collector_reg_handle(&self) -> Option<CollectorRegHandle> {
        self.cpu_collector_reg_handle.clone()
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for cq in self.completion_queues() {
            // it's safe to shutdown more than once.
            cq.shutdown()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_loop() {
        let mut env = Environment::new(2);

        let q1 = env.pick_cq();
        let q2 = env.pick_cq();
        let q3 = env.pick_cq();
        let cases = vec![(&q1, &q3, true), (&q1, &q2, false)];
        for (lq, rq, is_eq) in cases {
            let lq_ref = lq.borrow().unwrap();
            let rq_ref = rq.borrow().unwrap();
            if is_eq {
                assert_eq!(lq_ref.as_ptr(), rq_ref.as_ptr());
            } else {
                assert_ne!(lq_ref.as_ptr(), rq_ref.as_ptr());
            }
        }

        assert_eq!(env.completion_queues().len(), 2);
        for cq in env.completion_queues() {
            cq.shutdown();
        }

        for handle in env._handles.drain(..) {
            handle.join().unwrap();
        }
    }
}
