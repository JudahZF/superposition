//! A minimal Lambda-shaped host for `sp-rack-lane`, used by its tests and by the lane soak.
//!
//! Lambda runs a rack as an insert task in one layer of its DSP pool. In each block the task
//! reads the rack's output of the previous block, then writes the rack's input of this block;
//! the thread that completes the layer barrier wakes the rack's worker (the lead does it after
//! the pool run for the last layer). A rack is pipelined by one block.
//!
//! - [`Insert`] is that task: [`Insert::read`], [`Insert::write`], and [`Insert::wake`].
//! - [`RackControl`] is the plug-in supervisor: it starts one worker per segment, chains them,
//!   and replaces a failed worker on its other bank.
//! - [`echo`] is a plug-in-free worker that serves banks like `sp-plugin-worker`, so the
//!   adapter runs native, Windows (Wine), and mixed racks without VST3 bundles.
//!
//! The read deadline a request carries is the start of the next block: the earliest moment any
//! layer can read it. A host that reads every rack at the start of the next block and publishes
//! each at the end of its layer's admitted cost gives each rack its least window.

mod adapter;
pub mod echo;
pub mod histogram;
pub mod schedule;

pub use adapter::{
    ControlCounters, Insert, Launch, Platform, ProcessLaunch, RackControl, ReadOutcome,
    SegmentSpec, WineLaunch,
};
