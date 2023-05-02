// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Implementation of our efficient, single-threaded task scheduler.
//!
//! Our scheduler uses a pinned memory slab to store tasks ([SchedulerFuture]s).
//! As background tasks are polled, they notify task in our scheduler via the
//! [crate::page::WakerPage]s.

//======================================================================================================================
// Imports
//======================================================================================================================

use crate::scheduler::{
    page::{
        WakerPageRef,
        WakerRef,
    },
    pin_slab::PinSlab,
    waker64::{
        WAKER_BIT_LENGTH,
        WAKER_BIT_LENGTH_SHIFT,
    },
    SchedulerHandle,
    Task,
};
use ::bit_iter::BitIter;
use ::rand::{
    rngs::SmallRng,
    RngCore,
    SeedableRng,
};
use ::std::{
    cell::{
        Ref,
        RefCell,
        RefMut,
    },
    collections::HashMap,
    future::Future,
    pin::Pin,
    ptr::NonNull,
    rc::Rc,
    task::{
        Context,
        Poll,
        Waker,
    },
};

//======================================================================================================================
// Constants
//======================================================================================================================

/// Seed for the random number generator used to generate tokens.
/// This value was chosen arbitrarily.
#[cfg(debug_assertions)]
const SCHEDULER_SEED: u64 = 42;

//======================================================================================================================
// Structures
//======================================================================================================================

/// Task Scheduler
#[derive(Clone)]
pub struct Scheduler {
    /// Stores all the tasks that are held by the scheduler.
    tasks: Rc<RefCell<PinSlab<Box<dyn Task>>>>,
    /// Maps between externally meaningful ids and the offset of the task in the slab.
    task_ids: Rc<RefCell<HashMap<u64, usize>>>,
    /// Holds the waker bits for controlling task scheduling.
    pages: Rc<RefCell<Vec<WakerPageRef>>>,
    /// Small random number generator for tokens.
    id_gen: Rc<RefCell<SmallRng>>,
}

//======================================================================================================================
// Associate Functions
//======================================================================================================================

/// Associate Functions for Scheduler
impl Scheduler {
    /// Given a handle representing a future, remove the future from the scheduler returning it.
    pub fn take(&self, mut handle: SchedulerHandle) -> Box<dyn Task> {
        let pages: Ref<Vec<WakerPageRef>> = self.pages.borrow();
        // We should not have a scheduler handle that refers to an invalid id, so unwrap and expect are safe here.
        let offset: usize = *self
            .task_ids
            .borrow()
            .get(&handle.take_token().unwrap())
            .expect("Token should be in the token table");
        let (page, subpage_ix): (&WakerPageRef, usize) = {
            let (pages_ix, subpage_ix) = self.get_page_offsets(offset);
            (&pages[pages_ix], subpage_ix)
        };
        assert!(!page.was_dropped(subpage_ix));
        page.clear(subpage_ix);
        self.tasks.borrow_mut().remove_unpin(offset).unwrap()
    }

    /// Given the raw `token` representing this future return a proper handle.
    pub fn from_raw_handle(&self, task_id: u64) -> Option<SchedulerHandle> {
        let pages: Ref<Vec<WakerPageRef>> = self.pages.borrow();
        let offset: usize = match self.task_ids.borrow().get(&task_id) {
            Some(offset) => *offset,
            None => return None,
        };
        self.tasks.borrow().get(offset as usize)?;
        let page: &WakerPageRef = {
            let (pages_ix, _) = self.get_page_offsets(offset);
            &pages[pages_ix]
        };
        let handle: SchedulerHandle = SchedulerHandle::new(task_id, offset, page.clone());
        Some(handle)
    }

    /// Insert a new task into our scheduler returning a handle corresponding to it.
    pub fn insert<F: Task>(&self, future: F) -> Option<SchedulerHandle> {
        let mut pages: RefMut<Vec<WakerPageRef>> = self.pages.borrow_mut();
        let mut id_gen: RefMut<SmallRng> = self.id_gen.borrow_mut();
        // Allocate an offset into the slab and a token for identifying the task.
        let offset: usize = self.tasks.borrow_mut().insert(Box::new(future))?;

        // Generate a new id. If the id is currently in use, keep generating until we find an unused id.
        let id: u64 = loop {
            let id: u64 = id_gen.next_u64();
            if self.task_ids.borrow_mut().insert(id, offset).is_none() {
                break id;
            }
        };

        trace!(
            "scheduler::insert() inserting task with id={:?} and offset={:?}",
            id,
            offset
        );

        // Add a new page to hold this future's status if the current page is filled.
        while offset as usize >= pages.len() << WAKER_BIT_LENGTH_SHIFT {
            pages.push(WakerPageRef::default());
        }
        let (page, subpage_ix): (&WakerPageRef, usize) = {
            let (pages_ix, subpage_ix) = self.get_page_offsets(offset);
            (&pages[pages_ix], subpage_ix)
        };
        page.initialize(subpage_ix);
        let (page, _): (&WakerPageRef, usize) = {
            let (pages_ix, subpage_ix) = self.get_page_offsets(offset);
            (&pages[pages_ix], subpage_ix)
        };
        Some(SchedulerHandle::new(id, offset, page.clone()))
    }

    /// Computes the page and page offset of a given task based on its total offset.
    fn get_page_offsets(&self, offset: usize) -> (usize, usize) {
        (offset >> WAKER_BIT_LENGTH_SHIFT, offset & (WAKER_BIT_LENGTH - 1))
    }

    /// Poll all futures which are ready to run again. Tasks in our scheduler are notified when
    /// relevant data or events happen. The relevant event have callback function (the waker) which
    /// they can invoke to notify the scheduler that future should be polled again.
    pub fn poll(&self) {
        let mut pages: RefMut<Vec<WakerPageRef>> = self.pages.borrow_mut();
        let mut tasks: RefMut<PinSlab<Box<dyn Task>>> = self.tasks.borrow_mut();

        // Iterate through pages.
        for page_ix in 0..pages.len() {
            let (notified, dropped): (u64, u64) = {
                let page: &mut WakerPageRef = &mut pages[page_ix];
                (page.take_notified(), page.take_dropped())
            };
            // There is some notified task in this page, so iterate through it.
            if notified != 0 {
                for subpage_ix in BitIter::from(notified) {
                    // Handle notified tasks only.
                    // Get future using our page indices and poll it!
                    let ix: usize = (page_ix << WAKER_BIT_LENGTH_SHIFT) + subpage_ix;
                    let waker: Waker = unsafe {
                        let raw_waker: NonNull<u8> = pages[page_ix].into_raw_waker_ref(subpage_ix);
                        Waker::from_raw(WakerRef::new(raw_waker).into())
                    };
                    let mut sub_ctx: Context = Context::from_waker(&waker);

                    let pinned_ref: Pin<&mut Box<dyn Task>> = tasks.get_pin_mut(ix).unwrap();
                    let pinned_ptr = unsafe { Pin::into_inner_unchecked(pinned_ref) as *mut _ };

                    // Poll future.
                    drop(pages);
                    drop(tasks);
                    let pinned_ref = unsafe { Pin::new_unchecked(&mut *pinned_ptr) };
                    let poll_result: Poll<()> = Future::poll(pinned_ref, &mut sub_ctx);
                    pages = self.pages.borrow_mut();
                    tasks = self.tasks.borrow_mut();
                    match poll_result {
                        Poll::Ready(()) => pages[page_ix].mark_completed(subpage_ix),
                        Poll::Pending => (),
                    }
                }
            }
            // There is some dropped task in this page, so iterate through it.
            if dropped != 0 {
                // Handle dropped tasks only.
                for subpage_ix in BitIter::from(dropped) {
                    if subpage_ix != 0 {
                        let ix: usize = (page_ix << WAKER_BIT_LENGTH_SHIFT) + subpage_ix;
                        tasks.remove(ix);
                        pages[page_ix].clear(subpage_ix);
                    }
                }
            }
        }
    }
}

//======================================================================================================================
// Trait Implementations
//======================================================================================================================

/// Default Trait Implementation for Scheduler
impl Default for Scheduler {
    /// Creates a scheduler with default values.
    fn default() -> Self {
        Self {
            tasks: Rc::new(RefCell::new(PinSlab::new())),
            task_ids: Rc::new(RefCell::new(HashMap::<u64, usize>::new())),
            pages: Rc::new(RefCell::new(vec![])),
            #[cfg(debug_assertions)]
            id_gen: Rc::new(RefCell::new(SmallRng::seed_from_u64(SCHEDULER_SEED))),
            #[cfg(not(debug_assertions))]
            id_gen: Rc::new(RefCell::new(SmallRng::from_entropy())),
        }
    }
}

//======================================================================================================================
// Unit Tests
//======================================================================================================================

#[cfg(test)]
mod tests {
    use crate::scheduler::{
        scheduler::{
            Scheduler,
            SchedulerHandle,
        },
        task::TaskWithResult,
    };
    use ::anyhow::Result;
    use ::std::{
        future::Future,
        pin::Pin,
        task::{
            Context,
            Poll,
            Waker,
        },
    };
    use ::test::{
        black_box,
        Bencher,
    };

    #[derive(Default)]
    struct DummyCoroutine {
        pub val: usize,
    }

    impl DummyCoroutine {
        pub fn new(val: usize) -> Self {
            let f: Self = Self { val };
            f
        }
    }
    impl Future for DummyCoroutine {
        type Output = ();

        fn poll(self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
            match self.as_ref().val & 1 {
                0 => Poll::Ready(()),
                _ => {
                    self.get_mut().val += 1;
                    let waker: &Waker = ctx.waker();
                    waker.wake_by_ref();
                    Poll::Pending
                },
            }
        }
    }

    type DummyTask = TaskWithResult<()>;

    #[bench]
    fn bench_scheduler_insert(b: &mut Bencher) {
        let scheduler: Scheduler = Scheduler::default();

        b.iter(|| {
            let task: DummyTask =
                DummyTask::new(String::from("testing"), Box::pin(black_box(DummyCoroutine::default())));
            let handle: SchedulerHandle = scheduler.insert(task).expect("couldn't insert future in scheduler");
            black_box(handle);
        });
    }

    /// Tests if when inserting multiple tasks into the scheduler at once each, of them gets a unique identifier.
    #[test]
    fn test_scheduler_insert() -> Result<()> {
        let scheduler: Scheduler = Scheduler::default();

        // Insert a task and make sure the task id is not a simple counter.
        let task: DummyTask = DummyTask::new(String::from("testing"), Box::pin(DummyCoroutine::new(0)));
        let handle: SchedulerHandle = match scheduler.insert(task) {
            Some(handle) => handle,
            None => anyhow::bail!("insert() failed"),
        };
        let task_id: u64 = handle.into_raw();

        // Insert another task and make sure the task id is not sequentially after the previous one.
        let task2: DummyTask = DummyTask::new(String::from("testing"), Box::pin(DummyCoroutine::new(0)));
        let handle2: SchedulerHandle = match scheduler.insert(task2) {
            Some(handle) => handle,
            None => anyhow::bail!("insert() failed"),
        };
        let task_id2: u64 = handle2.into_raw();
        crate::ensure_neq!(task_id2, task_id + 1);

        Ok(())
    }

    #[test]
    fn scheduler_poll_once() -> Result<()> {
        let scheduler: Scheduler = Scheduler::default();

        // Insert a single future in the scheduler. This future shall complete with a single poll operation.
        let task: DummyTask = DummyTask::new(String::from("testing"), Box::pin(DummyCoroutine::new(0)));
        let handle: SchedulerHandle = match scheduler.insert(task) {
            Some(handle) => handle,
            None => anyhow::bail!("insert() failed"),
        };

        // All futures are inserted in the scheduler with notification flag set.
        // By polling once, our future should complete.
        scheduler.poll();

        crate::ensure_eq!(handle.has_completed(), true);

        Ok(())
    }

    #[test]
    fn scheduler_poll_twice() -> Result<()> {
        let scheduler: Scheduler = Scheduler::default();

        // Insert a single future in the scheduler. This future shall complete
        // with two poll operations.
        let task: DummyTask = DummyTask::new(String::from("testing"), Box::pin(DummyCoroutine::new(1)));
        let handle: SchedulerHandle = match scheduler.insert(task) {
            Some(handle) => handle,
            None => anyhow::bail!("insert() failed"),
        };

        // All futures are inserted in the scheduler with notification flag set.
        // By polling once, this future should make a transition.
        scheduler.poll();

        crate::ensure_eq!(handle.has_completed(), false);

        // This shall make the future ready.
        scheduler.poll();

        crate::ensure_eq!(handle.has_completed(), true);

        Ok(())
    }

    /// Tests if consecutive tasks are not assigned the same task id.
    #[test]
    fn test_scheduler_task_ids() -> Result<()> {
        let scheduler: Scheduler = Scheduler::default();

        // Create and run a task.
        let task: DummyTask = DummyTask::new(String::from("testing"), Box::pin(DummyCoroutine::new(0)));
        let handle: SchedulerHandle = match scheduler.insert(task) {
            Some(handle) => handle,
            None => anyhow::bail!("insert() failed"),
        };
        let task_id: u64 = handle.clone().into_raw();
        scheduler.poll();

        // Ensure that the first task has completed.
        crate::ensure_eq!(handle.has_completed(), true);

        // Create another task.
        let task2: DummyTask = DummyTask::new(String::from("testing"), Box::pin(DummyCoroutine::new(0)));
        let handle2: SchedulerHandle = match scheduler.insert(task2) {
            Some(handle) => handle,
            None => anyhow::bail!("insert() failed"),
        };
        let task_id2: u64 = handle2.into_raw();

        // Ensure that the second task has a unique id.
        crate::ensure_neq!(task_id2, task_id);

        Ok(())
    }

    #[bench]
    fn bench_scheduler_poll(b: &mut Bencher) {
        let scheduler: Scheduler = Scheduler::default();
        let mut handles: Vec<SchedulerHandle> = Vec::<SchedulerHandle>::with_capacity(1024);

        // Insert 1024 futures in the scheduler.
        // Half of them will be ready.
        for val in 0..1024 {
            let task: DummyTask = DummyTask::new(String::from("testing"), Box::pin(DummyCoroutine::new(val)));
            let handle: SchedulerHandle = match scheduler.insert(task) {
                Some(handle) => handle,
                None => panic!("insert() failed"),
            };
            handles.push(handle);
        }

        b.iter(|| {
            black_box(scheduler.poll());
        });
    }
}
