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

use crate::{
    collections::{
        id_map::IdMap,
        pin_slab::PinSlab,
    },
    runtime::scheduler::{
        page::{
            WakerPageRef,
            WakerRef,
        },
        scheduler::InternalId,
        waker64::{
            WAKER_BIT_LENGTH,
            WAKER_BIT_LENGTH_SHIFT,
        },
        Task,
        TaskId,
    },
};
use ::bit_iter::BitIter;
use ::futures::Future;
use ::std::{
    pin::Pin,
    ptr::NonNull,
    task::{
        Context,
        Poll,
        Waker,
    },
};

//======================================================================================================================
// Structures
//======================================================================================================================

/// This represents a resource management group. All tasks belong to a task group. By default, a task belongs to the
/// same task group as the allocating task.
#[derive(Default)]
pub struct TaskGroup {
    ids: IdMap<TaskId, InternalId>,
    /// Stores all the tasks that are held by the scheduler.
    tasks: PinSlab<Box<dyn Task>>,
    /// Holds the waker bits for controlling task scheduling.
    waker_page_refs: Vec<WakerPageRef>,
}

//======================================================================================================================
// Associate Functions
//======================================================================================================================

impl TaskGroup {
    /// Given a handle to a task, remove it from the scheduler
    pub fn remove(&mut self, task_id: TaskId) -> Option<Box<dyn Task>> {
        // We should not have a scheduler handle that refers to an invalid id, so unwrap and expect are safe here.
        let pin_slab_index: usize = self
            .ids
            .remove(&task_id)
            .expect("Token should be in the token table")
            .into();
        let (waker_page_ref, waker_page_offset): (&WakerPageRef, usize) = {
            let (waker_page_index, waker_page_offset) = self.get_waker_page_index_and_offset(pin_slab_index)?;
            (&self.waker_page_refs[waker_page_index], waker_page_offset)
        };
        waker_page_ref.clear(waker_page_offset);
        if let Some(task) = self.tasks.remove_unpin(pin_slab_index) {
            trace!(
                "remove(): name={:?}, id={:?}, pin_slab_index={:?}",
                task.get_name(),
                task_id,
                pin_slab_index
            );
            Some(task)
        } else {
            warn!(
                "Unable to unpin and remove: id={:?}, pin_slab_index={:?}",
                task_id, pin_slab_index
            );
            None
        }
    }

    /// Insert a new task into our scheduler returning a handle corresponding to it.
    pub fn insert(&mut self, task: Box<dyn Task>) -> Option<TaskId> {
        let task_name: String = task.get_name();
        // The pin slab index can be reverse-computed in a page index and an offset within the page.
        let pin_slab_index: usize = self.tasks.insert(task)?;
        let task_id: TaskId = self.ids.insert_with_new_id(pin_slab_index.into());

        self.add_new_pages_up_to_pin_slab_index(pin_slab_index.into());

        // Initialize the appropriate page offset.
        let (waker_page_ref, waker_page_offset): (&WakerPageRef, usize) = {
            let (waker_page_index, waker_page_offset) = self.get_waker_page_index_and_offset(pin_slab_index)?;
            (&self.waker_page_refs[waker_page_index], waker_page_offset)
        };
        waker_page_ref.initialize(waker_page_offset);

        trace!(
            "insert(): name={:?}, id={:?}, pin_slab_index={:?}",
            task_name,
            task_id,
            pin_slab_index
        );
        // Set this task's id.
        self.tasks
            .get_pin_mut(pin_slab_index)
            .expect("just allocated!")
            .set_id(task_id);
        Some(task_id)
    }

    /// Computes the page and page offset of a given task based on its total offset.
    fn get_waker_page_index_and_offset(&self, pin_slab_index: usize) -> Option<(usize, usize)> {
        // This check ensures that the slab slot is actually occupied but trusts that the pin_slab_index is for this
        // task.
        if !self.tasks.contains(pin_slab_index) {
            return None;
        }
        let waker_page_index: usize = pin_slab_index >> WAKER_BIT_LENGTH_SHIFT;
        let waker_page_offset: usize = Self::get_waker_page_offset(pin_slab_index);
        Some((waker_page_index, waker_page_offset))
    }

    /// Add new page(s) to hold this future's status if the current page is filled. This may result in addition of
    /// multiple pages because of the gap between the pin slab index and the current page index.
    fn add_new_pages_up_to_pin_slab_index(&mut self, pin_slab_index: usize) {
        while pin_slab_index >= (self.waker_page_refs.len() << WAKER_BIT_LENGTH_SHIFT) {
            self.waker_page_refs.push(WakerPageRef::default());
        }
    }

    pub fn get_num_waker_pages(&self) -> usize {
        self.waker_page_refs.len()
    }

    fn get_waker_page_offset(pin_slab_index: usize) -> usize {
        pin_slab_index & (WAKER_BIT_LENGTH - 1)
    }

    fn get_pin_slab_index(waker_page_index: usize, waker_page_offset: usize) -> usize {
        (waker_page_index << WAKER_BIT_LENGTH_SHIFT) + waker_page_offset
    }

    pub fn has_completed(&self, task_id: TaskId) -> Option<bool> {
        let pin_slab_index: usize = self.ids.get(&task_id)?.into();

        let (waker_page_ref, waker_page_offset) = {
            let (waker_page_index, waker_page_offset) = self.get_waker_page_index_and_offset(pin_slab_index)?;
            (&self.waker_page_refs[waker_page_index], waker_page_offset)
        };

        Some(waker_page_ref.has_completed(waker_page_offset))
    }

    pub fn get_offsets_for_ready_tasks(&mut self) -> Vec<usize> {
        let mut result: Vec<usize> = vec![];
        for i in 0..self.get_num_waker_pages() {
            // Grab notified bits.
            let notified: u64 = self.waker_page_refs[i].take_notified();
            // Turn into bit iter.
            let mut offset: Vec<usize> = BitIter::from(notified)
                .map(|x| Self::get_pin_slab_index(i, x))
                .collect();
            result.append(&mut offset);
        }
        result
    }

    pub fn get_id(&self, pin_slab_index: usize) -> TaskId {
        self.tasks
            .get(pin_slab_index)
            .expect(format!("Invalid offset: {:?}", pin_slab_index).as_str())
            .get_id()
    }

    fn get_pinned_task_ptr(&mut self, pin_slab_index: usize) -> Pin<&mut Box<dyn Task>> {
        // Get the pinned ref.
        self.tasks
            .get_pin_mut(pin_slab_index)
            .expect(format!("Invalid offset: {:?}", pin_slab_index).as_str())
    }

    fn get_waker(&self, waker_page_index: usize, waker_page_offset: usize) -> Waker {
        let raw_waker: NonNull<u8> = self.waker_page_refs[waker_page_index].into_raw_waker_ref(waker_page_offset);
        unsafe { Waker::from_raw(WakerRef::new(raw_waker).into()) }
    }

    pub fn poll_notified_task(&mut self, pin_slab_index: usize) -> Option<bool> {
        // Get the waker context.
        let (waker_page_index, waker_page_offset) = self.get_waker_page_index_and_offset(pin_slab_index)?;
        let waker: Waker = self.get_waker(waker_page_index, waker_page_offset);
        let mut waker_context: Context = Context::from_waker(&waker);

        let mut pinned_ptr = self.get_pinned_task_ptr(pin_slab_index);
        let pinned_ref = unsafe { Pin::new_unchecked(&mut *pinned_ptr) };

        // Poll future.
        let poll_result: Poll<()> = Future::poll(pinned_ref, &mut waker_context);
        if let Poll::Ready(()) = poll_result {
            self.waker_page_refs[waker_page_index].mark_completed(waker_page_offset);
            Some(true)
        } else {
            Some(false)
        }
    }

    #[cfg(test)]
    pub fn is_valid_task(&self, task_id: &TaskId) -> bool {
        if let Some(internal_id) = self.ids.get(task_id) {
            self.tasks.contains(internal_id.into())
        } else {
            false
        }
    }

    #[cfg(test)]
    pub fn num_tasks(&self) -> usize {
        self.ids.len()
    }
}
