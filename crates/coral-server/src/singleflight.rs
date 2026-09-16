//! Singleflight 并发去重。
//!
//! 同一 key 并发调用只执行一次闭包；等待者被唤醒后返回
//! [`Outcome::Deduplicated`]，由调用方重查缓存（语义：
//! "notify.notified().await 后重查缓存"）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// 单次调用结果：执行了闭包，还是被去重（需重查缓存）。
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome<T> {
    /// 本调用执行了闭包
    Executed(T),
    /// 同 key 已有在途执行，等待其完成后被唤醒——调用方应重查缓存
    Deduplicated,
}

#[derive(Default)]
pub struct Singleflight {
    inflight: Mutex<HashMap<String, Arc<Notify>>>,
}

impl Singleflight {
    /// 对 `key` 执行 `f`；并发同 key 只有一个执行。
    /// 等待者等到的是 `Deduplicated`，不重复执行闭包。
    pub async fn do_once<F, Fut, T>(&self, key: String, f: F) -> Outcome<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        // 拿执行权或注册等待
        let waiter: Option<Arc<Notify>> = {
            let mut map = self.inflight.lock().expect("singleflight 锁中毒");
            match map.get(&key) {
                Some(notify) => Some(notify.clone()),
                None => {
                    map.insert(key.clone(), Arc::new(Notify::new()));
                    None
                }
            }
        };

        if let Some(notify) = waiter {
            notify.notified().await;
            return Outcome::Deduplicated;
        }

        // 执行者：完成或失败都要清 entry + 唤醒等待者
        let result = f().await;
        if let Some(notify) = {
            let mut map = self.inflight.lock().expect("singleflight 锁中毒");
            map.remove(&key)
        } {
            notify.notify_waiters();
        }
        Outcome::Executed(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn test_concurrent_calls_execute_once() {
        let sf = Arc::new(Singleflight::default());
        let executions = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..50 {
            let sf = sf.clone();
            let counter = executions.clone();
            handles.push(tokio::spawn(async move {
                sf.do_once("k".to_string(), move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        "done"
                    }
                })
                .await
            }));
        }
        let mut executed = 0;
        for h in handles {
            match h.await.expect("task panic") {
                Outcome::Executed(_) => executed += 1,
                Outcome::Deduplicated => {}
            }
        }
        assert_eq!(executions.load(Ordering::SeqCst), 1, "闭包只执行 1 次");
        assert_eq!(executed, 1, "只有一个调用者拿到 Executed");
    }

    #[tokio::test]
    async fn test_sequential_calls_both_execute() {
        let sf = Singleflight::default();
        let a = sf.do_once("k".to_string(), || async { 1 }).await;
        assert_eq!(a, Outcome::Executed(1));
        let b = sf.do_once("k".to_string(), || async { 2 }).await;
        assert_eq!(b, Outcome::Executed(2), "无在途执行时新调用照常执行");
    }
}
