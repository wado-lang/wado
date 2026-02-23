// Copyright 2025 International Digital Economy Academy
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

///|
priv struct Reader[X] {
  /// the value provided the reader.
  /// `value` must be `Some(_)` when the reader is woken
  mut value : X?
  /// `None` indicates that the reader is cancelled
  mut coro : @coroutine.Coroutine?
}

///|
priv struct Writer[X] {
  value : X
  mut coro : @coroutine.Coroutine?
}

///|
/// Options for different behavior of `put` in async queue.
/// - `Unbounded`: the queue has unbounded size, so `put` always succeed and never blocks.
///   However, if the reader of the queue hangs silently,
///   elements will accumulate infinitely in the queue, causing memory leak
/// - `Blocking(n)`: the queue can contain at most `n` elements,
///   if the queue is full, `put` will block until some value is consumed.
///   If multiple blocked writers coexist, they will be accepted in a FIFO manner
/// - `DiscardOldest(n)`: the queue can contain at most `n` elements,
///   if the queue is full, the oldest elements will be discarded,
///   so `put` always succeed and never block
/// - `DiscardLatest(n)`: the queue can contain at most `n` elements,
///   if the queue is full, the newest elements, i.e. the argument of `put`,
///   will be silently discarded.
pub(all) enum Kind {
  Unbounded
  Blocking(Int)
  DiscardOldest(Int)
  DiscardLatest(Int)
}

///|
/// An asynchronous queue, where reader can wait for data to arrive
/// in a non-blocking manner.
struct Queue[X] {
  kind : Kind
  readers : @deque.Deque[Reader[X]]
  writers : @deque.Deque[Writer[X]]
  /// invariant: if `buffer` is non-empty, `readers` must be empty
  buffer : @deque.Deque[X]
  /// `Some(err)` if the queue is already closed
  mut closed : Error?
}

///|
/// Create an empty queue.
/// The behavior of `put` is determined by the `kind` argument,
/// see the type `@aqueue.Kind` for more details.
#as_free_fn
#callsite(migration(kind, fill=true, msg="specify the kind of queue explicitly via `kind=...`"))
pub fn[X] Queue::new(kind? : Kind = Unbounded) -> Queue[X] {
  match kind {
    Unbounded => ()
    Blocking(n) | DiscardOldest(n) | DiscardLatest(n) =>
      if n <= 0 {
        abort("queue size must be positive")
      }
  }
  {
    kind,
    writers: @deque.new(),
    readers: @deque.new(),
    buffer: @deque.new(),
    closed: None,
  }
}

///|
/// Put a new element into a queue.
///
/// - if there are readers blocked on this thread,
///   the first reader will be woken to process the data
/// - if there are no readers currently blocking,
///   but the queue still have enough spare space,
///   the new element will be placed in the queue,
///   and `put` will return immediately without suspension
/// - if the queue is already full, the behavior is determined by
///   the `kind` argument provided on queue creation:
///     - `Unbounded`: the queue has unbounded size, so can never be full
///     - `DiscardOldest`: the oldest element in the queue is discarded
///     - `DiscardLatest`: the argument of `put` is silently discarded,
///       and `put` will have not effect
///   
/// Although `put` is an async function,
/// if will not suspend unless the queue is `Blocking` and already full.
/// So for `Unbounded`/`DiscardOldest`/`DiscardLatest`,
/// it is safe to assume that `put` is actually synchronous.
/// For context where async function is not allowed, use `try_put` instead.
///
/// `put` itself never fails. But for `Blocking` queues,
/// `put` may be cancelled before it succeed,
/// in this case `put` will fail with a cancellation error.
/// It is guaranteed that the element will be added to the queue
/// *if and only if* `put` return normally.
///
/// If the queue is already closed, `put` will fail immediately.
/// If the queue is a blocking queue, and is closed while `put` is blocking,
/// `put` will also fail immediately.
pub async fn[X] Queue::put(self : Queue[X], data : X) -> Unit {
  if self.closed is Some(err) {
    raise err
  }
  if !self.try_put(data) {
    match self.kind {
      Unbounded => panic()
      Blocking(_) => {
        let writer = Writer::{
          value: data,
          coro: Some(@coroutine.current_coroutine()),
        }
        self.writers.push_back(writer)
        @coroutine.suspend() catch {
          _ if writer.coro is None => ()
          err => {
            writer.coro = None
            raise err
          }
        }
        if self.closed is Some(err) {
          raise err
        }
      }
      DiscardOldest(_) => {
        let _ = self.buffer.pop_front()
        self.buffer.push_back(data)
      }
      DiscardLatest(_) => ()
    }
  }
}

///|
/// Try to put a new element into an async queue.
/// If the queue still have enough room, the element will be added to the queue,
/// and `true` will be returned.
/// If the queue is already full,
/// the element will not be added and `false` will be returned.
/// Note that even for `DiscardOldest` or `DiscardLatest` queues,
/// where `put` never blocks, `try_put` will still return `false` if the queue is full,
/// instead of discarding the oldest/newest element.
///
/// If the queue is already closed, `try_put` will fail with error immediately.
pub fn[X] Queue::try_put(self : Queue[X], data : X) -> Bool raise {
  if self.closed is Some(err) {
    raise err
  }
  loop self.readers.pop_front() {
    None =>
      match self.kind {
        Unbounded => {
          self.buffer.push_back(data)
          true
        }
        Blocking(n) | DiscardOldest(n) | DiscardLatest(n) =>
          if self.buffer.length() < n {
            self.buffer.push_back(data)
            true
          } else {
            false
          }
      }
    Some({ coro: None, .. }) => continue self.readers.pop_front()
    Some({ coro: Some(coro), .. } as reader) => {
      // after waking the reader, it may not start execution immediately.
      // And a lot of things could happen in the middle.
      // So instead of using `self.buffer`,
      // we pass `data` to the woker reader in a point-to-point manner
      // to avoid some edge cases.
      reader.value = Some(data)
      coro.wake()
      true
    }
  }
}

///|
/// Fetch an element from the queue.
/// If the queue is currently empty, `get` will block and wait until data arrive.
/// If there are multiple readers blocked on `get`,
/// new data will be delivered in a first-come-first-serve manner.
///
/// `get` itself never fail, and will wait indefinitely.
/// However, since `get` is a blocking point, the task running `get` may be cancelled,
/// in this case, an cancellation will be raised from `get`.
///
/// If the queue is already closed and there is no buffered elements,
/// `get` will fail immediately.
/// If the queue is closed while `get` is waiting, `get` will also fail immediately.
pub async fn[X] Queue::get(self : Queue[X]) -> X {
  if self.try_get() is Some(data) {
    return data
  }
  let reader = Reader::{
    value: None,
    coro: Some(@coroutine.current_coroutine()),
  }
  self.readers.push_back(reader)
  @coroutine.suspend() catch {
    _ if reader.value is Some(value) =>
      // This may happen if:
      //
      // - `put` is called, the value is removed from the queue
      //   and assigned to `reader.value`
      // - the task is called after `put`, before `get` is actually woken
      //
      // In this case, if we cancel `get`,
      // the value would be swollen, which is incorrect.
      // It is also difficult to put the value back to the queue
      // while maintaining correct order.
      // So we choose to swallow the cancellation signal here,
      // which is fine because cancellation is level-triggered.
      return value
    err => {
      reader.coro = None
      raise err
    }
  }
  if self.closed is Some(err) {
    raise err
  }
  reader.value.unwrap()
}

///|
/// Try to fetch an element from the queue without blocking.
/// If no element is in the queue at the moment, `None` is returned.
///
/// If the queue is already closed and there is no buffered elements,
/// `try_get` will fail immediately.
pub fn[X] Queue::try_get(self : Queue[X]) -> X? raise {
  let result = self.buffer.pop_front()
  if result is Some(_) {
    loop self.writers.pop_front() {
      None => ()
      Some({ coro: None, .. }) => continue self.writers.pop_front()
      Some({ coro: Some(coro), value } as writer) => {
        self.buffer.push_back(value)
        coro.wake()
        writer.coro = None
      }
    }
  } else if self.closed is Some(err) {
    raise err
  }
  result
}

///|
/// Error raised when operating on a closed queue.
pub suberror QueueAlreadyClosed derive(Show, ToJson)

///|
/// Close an async queue. After the queue is closed:
///
/// - blocking `put` and `get` operations will fail immediately.
/// - subsequent `put` and `try_put` operations also fail immediately
/// - if `clear = false` (`false` by default),
///   sbusequent `get` and `try_get` operations can still retrieve
///   buffered elements in the queue before `close`.
///   After the queue becomes empty,
///   subsequent `get` and `try_get` operations also fail immediately
/// - if `clear = true`, buffered elements in the queue will be cleared
///
/// By default, writing to or reading from a closed queue
/// will receive the `QueueAlreadyClosed` error.
/// But this error can be customized via the `error` parameter.
pub fn[X] Queue::close(
  self : Queue[X],
  error? : Error = QueueAlreadyClosed,
  clear? : Bool = false,
) -> Unit {
  self.closed = Some(error)
  for writer in self.writers {
    if writer.coro is Some(coro) {
      coro.wake()
    }
  }
  for reader in self.readers {
    if reader.coro is Some(coro) {
      coro.wake()
    }
  }
  if clear {
    self.buffer.clear()
  }
}
