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
priv enum State {
  Done
  Fail(Error)
  Running
  Suspend(ok_cont~ : (Unit) -> Unit, err_cont~ : (Error) -> Unit)
}

///|
/// Coroutine handle for async scheduling.
struct Coroutine {
  coro_id : Int
  mut state : State
  mut shielded : Bool
  mut cancelled : Bool
  mut ready : Bool
  downstream : Set[Coroutine]
}

///|
pub impl Eq for Coroutine with equal(c1, c2) {
  c1.coro_id == c2.coro_id
}

///|
pub impl Hash for Coroutine with hash_combine(self, hasher) {
  self.coro_id.hash_combine(hasher)
}

///|
/// Wake a coroutine and schedule it to run.
pub fn Coroutine::wake(self : Coroutine) -> Unit {
  self.ready = true
  scheduler.run_later.push_back(self)
}

///|
/// Return whether the current coroutine is cancelled and not shielded.
pub fn is_being_cancelled() -> Bool {
  let coro = current_coroutine()
  coro.cancelled && not(coro.shielded)
}

///|
/// Cancellation error raised when a coroutine is cancelled.
pub(all) suberror Cancelled derive(Show)

///|
/// Mark a coroutine as cancelled and wake it if needed.
pub fn Coroutine::cancel(self : Coroutine) -> Unit {
  self.cancelled = true
  if not(self.shielded || self.ready) {
    self.wake()
  }
}

///|
/// Yield execution to other coroutines.
pub async fn pause() -> Unit {
  guard scheduler.curr_coro is Some(coro)
  if coro.cancelled && not(coro.shielded) {
    raise Cancelled
  }
  async_suspend(fn(ok_cont, err_cont) {
    guard coro.state is Running
    coro.state = Suspend(ok_cont~, err_cont~)
    coro.ready = true
    scheduler.run_later.push_back(coro)
  })
}

///|
/// Suspend the current coroutine until resumed by the scheduler.
pub async fn suspend() -> Unit {
  guard scheduler.curr_coro is Some(coro)
  if coro.cancelled && not(coro.shielded) {
    raise Cancelled
  }
  scheduler.blocking += 1
  defer {
    scheduler.blocking -= 1
  }
  async_suspend(fn(ok_cont, err_cont) {
    guard coro.state is Running
    coro.state = Suspend(ok_cont~, err_cont~)
  })
}

///|
/// Spawn a new coroutine for the async function.
pub fn spawn(f : async () -> Unit) -> Coroutine {
  scheduler.coro_id += 1
  let coro = {
    state: Running,
    ready: true,
    shielded: true,
    downstream: Set::new(),
    coro_id: scheduler.coro_id,
    cancelled: false,
  }
  fn run(_) {
    run_async(fn() {
      coro.shielded = false
      try f() catch {
        err => coro.state = Fail(err)
      } noraise {
        _ => coro.state = Done
      }
      for coro in coro.downstream {
        coro.wake()
      }
      coro.downstream.clear()
    })
  }

  coro.state = Suspend(ok_cont=run, err_cont=_ => ())
  scheduler.run_later.push_back(coro)
  coro
}

///|
/// Raise the coroutine error if it failed.
pub fn Coroutine::unwrap(self : Coroutine) -> Unit raise {
  match self.state {
    Done => ()
    Fail(err) => raise err
    Running | Suspend(_) => panic()
  }
}

///|
/// Wait for another coroutine to complete.
pub async fn Coroutine::wait(target : Coroutine) -> Unit {
  guard scheduler.curr_coro is Some(coro)
  guard not(physical_equal(coro, target))
  match target.state {
    Done => return
    Fail(err) => raise err
    Running | Suspend(_) => ()
  }
  target.downstream.add(coro)
  try suspend() catch {
    err => {
      target.downstream.remove(coro)
      raise err
    }
  } noraise {
    _ => target.unwrap()
  }
}

///|
/// Check and raise the error for a coroutine, if any.
pub fn Coroutine::check_error(coro : Coroutine) -> Unit raise {
  match coro.state {
    Fail(err) => raise err
    Done | Running | Suspend(_) => ()
  }
}

///|
/// Run an async function shielded from cancellation.
pub async fn protect_from_cancel(f : async () -> Unit) -> Unit {
  guard scheduler.curr_coro is Some(coro)
  if coro.shielded {
    // already in a shield, do nothing
    f()
  } else {
    coro.shielded = true
    defer {
      coro.shielded = false
    }
    f()
    if coro.cancelled {
      raise Cancelled
    }
  }
}

///|
/// Return the coroutine id.
pub fn Coroutine::id(coroutine : Coroutine) -> Int {
  coroutine.coro_id
}
