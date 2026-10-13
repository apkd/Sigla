//! Cooperative group commit. Callers retain their own backpressure and wait for
//! durability; the caller holding the writer drains already-queued batches.
use super::*;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

type Outcome = Result<Vec<Option<Installed>>>;

struct Request {
    scope: Scope,
    pending: Vec<PendingInstall>,
    queued: Instant,
    result: Mutex<Option<Outcome>>,
}

#[derive(Default)]
pub(super) struct Queue {
    waiting: VecDeque<Arc<Request>>,
    active: bool,
}

// These bound a transaction, not the producer's admission or an individual
// oversized object. An oversized request is accepted alone, as before.
const GROUP_BYTES: u64 = 32 * 1024 * 1024;
const GROUP_FILES: usize = 4096;

struct Leader<'a> {
    database: &'a Database,
    requests: Vec<Arc<Request>>,
    completed: bool,
}
impl Drop for Leader<'_> {
    fn drop(&mut self) {
        // Wake all participants even if the elected caller unwinds.
        if !self.completed {
            for request in &self.requests {
                let mut result = request.result.lock().unwrap();
                if result.is_none() {
                    *result = Some(Err(anyhow::anyhow!("Analysis publication interrupted")));
                }
            }
        }
        self.database.publications.lock().unwrap().active = false;
        self.database.publication_changed.notify_all();
    }
}

impl Database {
    pub(super) fn publish(&self, scope: &Scope, pending: Vec<PendingInstall>) -> Outcome {
        let request = Arc::new(Request {
            scope: scope.clone(),
            pending,
            queued: Instant::now(),
            result: Mutex::new(None),
        });
        let mut queue = self.publications.lock().unwrap();
        queue.waiting.push_back(request.clone());
        self.publication_changed.notify_all();
        loop {
            if let Some(result) = request.result.lock().unwrap().take() {
                return result;
            }
            if queue.active {
                queue = self.publication_changed.wait(queue).unwrap();
                continue;
            }
            queue.active = true;
            drop(queue);
            let mut leader = Leader {
                database: self,
                requests: Vec::new(),
                completed: false,
            };
            let queued = Instant::now();
            let writer = self.write();
            let wait = queued.elapsed();
            // Collect after acquiring the writer, so batches arriving during an
            // earlier commit join this one. There is no timer or extra thread.
            {
                let mut queue = self.publications.lock().unwrap();
                let mut bytes = 0u64;
                let mut files = 0usize;
                while let Some(next) = queue.waiting.front() {
                    let next_bytes = next.pending.iter().map(PendingInstall::bytes).sum::<u64>();
                    if !leader.requests.is_empty()
                        && (bytes.saturating_add(next_bytes) > GROUP_BYTES
                            || files.saturating_add(next.pending.len()) > GROUP_FILES)
                    {
                        break;
                    }
                    bytes = bytes.saturating_add(next_bytes);
                    files = files.saturating_add(next.pending.len());
                    leader.requests.push(queue.waiting.pop_front().unwrap());
                }
            }
            let outcomes = leader.apply(writer, wait);
            for (request, outcome) in leader.requests.iter().zip(outcomes) {
                *request.result.lock().unwrap() = Some(outcome);
            }
            leader.completed = true;
            drop(leader);
            queue = self.publications.lock().unwrap();
        }
    }
}

impl<'a> Leader<'a> {
    fn apply(&self, writer: Result<Writer<'a>>, mut wait: Duration) -> Vec<Outcome> {
        let mut results: Vec<Option<Outcome>> = (0..self.requests.len()).map(|_| None).collect();
        let applying = Instant::now();
        let mut request_wait = Duration::ZERO;
        let mut request_wait_max = Duration::ZERO;
        for request in &self.requests {
            let elapsed = applying.duration_since(request.queued);
            request_wait += elapsed;
            request_wait_max = request_wait_max.max(elapsed);
        }
        let mut writer = writer;
        let mut apply = Duration::ZERO;
        let mut commit = Duration::ZERO;
        let mut rejected = 0;
        let mut committed = false;
        loop {
            let mut tx = match writer {
                Ok(tx) => tx,
                Err(error) => {
                    fail_remaining(&mut results, error);
                    break;
                }
            };
            let applying = Instant::now();
            let mut values = Vec::new();
            let mut failed = false;
            for (index, request) in self.requests.iter().enumerate() {
                if results[index].is_some() {
                    continue;
                }
                match request.scope.apply_batch_in(&mut tx, &request.pending) {
                    Ok(value) => values.push((index, value)),
                    Err(error) => {
                        results[index] = Some(Err(error));
                        rejected += 1;
                        failed = true;
                        break;
                    }
                }
            }
            apply += applying.elapsed();
            if failed {
                // install_in may have modified the transaction before failing.
                // Abort the WHOLE transaction, then replay surviving requests.
                // A request remains atomic and a bad workspace cannot poison
                // an unrelated workspace's publication.
                drop(tx);
                if results.iter().all(Option::is_some) {
                    break;
                }
                let queued = Instant::now();
                writer = self.database.write();
                wait += queued.elapsed();
                continue;
            }
            let committing = Instant::now();
            match tx.commit() {
                Ok(()) => {
                    committed = true;
                    for (index, value) in values {
                        results[index] = Some(Ok(value));
                    }
                }
                Err(error) => fail_remaining(&mut results, error),
            }
            commit += committing.elapsed();
            break;
        }
        tracing::debug!(
            batches = self.requests.len(),
            files = self.requests.iter().map(|r| r.pending.len()).sum::<usize>(),
            rejected,
            committed,
            // Request waits include publication-queue and writer waits. They
            // overlap; wait_ms counts only the leader's writer reservations.
            request_wait_ms = request_wait.as_millis(),
            request_wait_max_ms = request_wait_max.as_millis(),
            wait_ms = wait.as_millis(),
            apply_ms = apply.as_millis(),
            commit_ms = commit.as_millis(),
            "analysis batch published"
        );
        results.into_iter().map(Option::unwrap).collect()
    }
}

fn fail_remaining(results: &mut [Option<Outcome>], error: anyhow::Error) {
    let message = format!("{error:#}");
    let mut error = Some(error);
    for result in results.iter_mut().filter(|result| result.is_none()) {
        *result = Some(Err(error
            .take()
            .unwrap_or_else(|| anyhow::anyhow!("{message}"))));
    }
}

impl Scope {
    fn apply_batch_in(&self, tx: &mut RwTxn<'_>, pending: &[PendingInstall]) -> Outcome {
        let mut installed = Vec::with_capacity(pending.len());
        for pending in pending {
            let result = self.install_in(tx, pending)?;
            if result.is_some()
                && let Some(input) = pending.input
            {
                let key = record_key(&input, b"input");
                if self.database.control.get(tx, &key)? != Some(pending.object.as_slice()) {
                    self.database.control.put(tx, &key, &pending.object)?;
                    let mut key = record_key(&pending.object, INPUT_ALIAS);
                    key.extend_from_slice(&input);
                    self.database.records.put(tx, &key, &[])?;
                }
            }
            installed.push(result);
        }
        Ok(installed)
    }

    /// Keep valid neighbors in a batch when individual input fences fail.
    /// Transactional failures are isolated by bisecting already-prepared work;
    /// no encoding is reconstructed and install_batch itself remains atomic.
    pub(crate) fn install_batch_resilient(
        &self,
        pending: &[PendingInstall],
        mut verify: impl FnMut(usize) -> Result<()>,
    ) -> Vec<Result<Option<Installed>>> {
        let mut results: Vec<_> = (0..pending.len()).map(|_| Ok(None)).collect();
        let indices: Vec<_> = (0..pending.len()).collect();
        self.publish_subset(pending, &indices, &mut results, &mut verify);
        results
    }

    fn publish_subset(
        &self,
        pending: &[PendingInstall],
        indices: &[usize],
        results: &mut [Result<Option<Installed>>],
        verify: &mut impl FnMut(usize) -> Result<()>,
    ) {
        // Each retry may follow a publication wait. Do not reuse input-fence
        // results from an earlier attempt, even for an unchanged neighbor.
        let indices: Vec<_> = indices
            .iter()
            .copied()
            .filter(|&index| match verify(index) {
                Ok(()) => true,
                Err(error) => {
                    results[index] = Err(error);
                    false
                }
            })
            .collect();
        if indices.is_empty() {
            return;
        }
        let batch: Vec<_> = indices
            .iter()
            .map(|&index| pending[index].clone())
            .collect();
        let installed = self.database.publish(self, batch);
        match installed {
            Ok(installed) => {
                for (&index, installed) in indices.iter().zip(installed) {
                    results[index] = Ok(installed);
                }
            }
            Err(_) if indices.len() == 1 => {
                let index = indices[0];
                // The former per-file fallback prepared a fresh expectation.
                // Preserve that recovery, but retain the immutable encoding.
                // Recheck the input before accepting a newer binding and again
                // before publication; a changed source must never be overwritten.
                results[index] = (|| -> Result<_> {
                    verify(index)?;
                    let mut fresh = pending[index].clone();
                    {
                        let tx = self.read()?;
                        fresh.expected = self.binding(&tx, &fresh.file)?;
                    }
                    let mut installed = self.install_batch(&[fresh], |_| verify(index))?;
                    Ok(installed.pop().unwrap())
                })();
            }
            Err(_) => {
                let (left, right) = indices.split_at(indices.len() / 2);
                self.publish_subset(pending, left, results, verify);
                self.publish_subset(pending, right, results, verify);
            }
        }
    }
}

#[cfg(test)]
#[path = "publication_tests.rs"]
mod tests;
