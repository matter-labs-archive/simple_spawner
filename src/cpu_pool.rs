extern crate futures;
extern crate num_cpus;

use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::fmt;
use std::future::{Future};
use std::pin::Pin;

use futures::task::Spawn;
use futures::future::{FutureObj, FutureExt};
use futures::channel::oneshot::{Receiver, Sender, channel};
use futures::future::{lazy};
use futures::executor::{ThreadPool, ThreadPoolBuilder, block_on};
use futures::task::{Poll, Context};

pub struct CpuPool {
    size: usize,
    executor: Arc<Mutex<ThreadPool>>
}

pub struct Builder {
    pool_size: usize,
    stack_size: usize,
    name_prefix: Option<String>,
}

struct ResultSender<F: FutureExt, T> {
    fut: F,
    tx: Option<Sender<T>>,
    keep_running_flag: Arc<AtomicBool>,
}

trait AssertSendSync: Send + Sync {}
impl AssertSendSync for CpuPool {}

impl fmt::Debug for CpuPool {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("CpuPool")
            .field("size", &self.size)
            .finish()
    }
}

impl fmt::Debug for Builder {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Builder")
            .field("pool_size", &self.pool_size)
            .field("name_prefix", &self.name_prefix)
            .finish()
    }
}

#[must_use]
#[derive(Debug)]
pub struct CpuFuture<T, E> {
    result_receiver: Receiver<thread::Result<Result<T, E>>>,
    keep_running_flag: Arc<AtomicBool>,
}

impl CpuPool {
    pub fn new(size: usize) -> CpuPool {
        Builder::new().pool_size(size).create()
    }

    pub fn new_num_cpus() -> CpuPool {
        Builder::new().create()
    }

    pub fn spawn<F, T, E>(&mut self, f: F) -> CpuFuture<T, E>
        where F: Future<Output = Result<T, E>> + Send + 'static,
              F::Output: Send + 'static,
              T: Send + 'static,
              E: Send + 'static
    {
        let (tx, rx) = channel();
        let keep_running_flag = Arc::new(AtomicBool::new(false));
        let sender = ResultSender {
            fut: AssertUnwindSafe(f).catch_unwind(),
            tx: Some(tx),
            keep_running_flag: Arc::clone(&keep_running_flag),
        };

        let future_obj: FutureObj<_> = Box::from(sender).into();

        self.executor.lock().expect("will lock eventually").spawn_obj(future_obj).expect("should spawn");

        CpuFuture { result_receiver: rx , keep_running_flag: keep_running_flag.clone() }
    }

    pub fn spawn_fn<F, T, E>(&mut self, f: F) -> CpuFuture<T, E>
        where F: FnOnce() -> Result<T, E> + Send + 'static,
                T: Send + 'static,
                E: Send + 'static
    {
        let lazy_future = lazy(|_| f() );
        self.spawn(lazy_future)
    }
}

impl Clone for CpuPool {
    fn clone(&self) -> CpuPool {
        CpuPool { size: self.size, executor: self.executor.clone() }
    }
}

impl<T, E> CpuFuture<T, E> {
    pub fn forget(self) {
        self.keep_running_flag.store(true, Ordering::SeqCst);
    }
}

impl<T: Send + 'static, E: Send + 'static> CpuFuture<T, E> {
    pub fn wait(self) -> Result<T, E> {
        let result = block_on(self);
        result
    }
}

impl<T: Send + 'static, E: Send + 'static> Future for CpuFuture<T, E> {
    type Output = Result<T, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let rec = unsafe { self.map_unchecked_mut(|s| &mut s.result_receiver) };
        match rec.poll(cx) {
            Poll::Ready(Ok(Ok(e))) => Poll::Ready(e.into()),
            Poll::Ready(Ok(Err(e))) => panic::resume_unwind(e),
            Poll::Ready(Err(_e)) => {
                unreachable!();
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<F: FutureExt> Future for ResultSender<F, F::Output> 
    where F::Output: Send + 'static 
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let s = unsafe { self.get_unchecked_mut() };

        if let Poll::Ready(_) = s.tx.as_mut().expect("can take this").poll_cancel(cx) {
            if !s.keep_running_flag.load(Ordering::SeqCst) {
                // Cancelled, bail out
                return Poll::Ready(());
            }
        }

        let fut = unsafe { Pin::new_unchecked(&mut s.fut)};

        let res = match fut.poll(cx) {
            Poll::Ready(e) => e,
            Poll::Pending => return Poll::Pending,
        };

        drop(s.tx.take().expect("receiver exists").send(res));
        Poll::Ready(())
    }
}

impl Builder {
    pub fn new() -> Builder {
        Builder {
            pool_size: num_cpus::get(),
            stack_size: 0,
            name_prefix: None,
        }
    }

    pub fn pool_size(&mut self, size: usize) -> &mut Self {
        self.pool_size = size;
        self
    }

    /// Set stack size of threads in the pool.
    pub fn stack_size(&mut self, stack_size: usize) -> &mut Self {
        self.stack_size = stack_size;
        self
    }

    pub fn name_prefix<S: Into<String>>(&mut self, name_prefix: S) -> &mut Self {
        self.name_prefix = Some(name_prefix.into());
        self
    }

    pub fn create(&mut self) -> CpuPool {
        assert!(self.pool_size > 0);

        let mut builder = ThreadPoolBuilder::new();

        let mut executor_builder = (&mut builder).pool_size(self.pool_size);
        if let Some(ref name_prefix) = self.name_prefix {
            executor_builder = executor_builder.name_prefix(format!("{}", name_prefix));
        }
        if self.stack_size > 0 {
            executor_builder = executor_builder.stack_size(self.stack_size);
        }

        let pool = CpuPool {
            size: self.pool_size,
            executor: Arc::new(
                Mutex::from(executor_builder.create().expect("pool creation should be ok"))
            ),
        };
        
        return pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drop_after_start() {
        fn dummy_fn(sleep_ms: u64) -> Result<usize, ()>{
            println!("Will sleep {}ms", sleep_ms);
            std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
            println!("Will return");
            Ok(10usize)
        }

        let mut cpu_pool = CpuPool::new(8);
        let fut = cpu_pool.spawn_fn(|| dummy_fn(10000));
        let mut results = vec![];
        for i in 0u64..16u64 {
            let r = cpu_pool.spawn_fn(move || dummy_fn(1000*i));
            results.push(r);
        }
        println!("Spawned");
        println!("Will sleep before polling");
        std::thread::sleep(std::time::Duration::from_millis(60000));
        let result = fut.wait();
        println!("Got result {:?}", result);
    }
}