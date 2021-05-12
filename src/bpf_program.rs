// SPDX-License-Identifier: GPL-2.0-or-later
//
// BPFContain - Container security with eBPF
// Copyright (C) 2020  William Findlay
//
// Dec. 29, 2020  William Findlay  Created this.

//! Functionality related to BPF programs and maps.

use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result};
use glob::glob;
use libbpf_rs::{RingBuffer, RingBufferBuilder};

use crate::bpf::{BpfcontainSkel, BpfcontainSkelBuilder, OpenBpfcontainSkel};
use crate::ns;
use crate::policy::Policy;
use crate::uprobe_ext::FindSymbolUprobeExt;
use crate::uprobes::do_containerize;
use crate::utils::bump_memlock_rlimit;

pub struct BpfcontainContext<'a> {
    skel: BpfcontainSkel<'a>,
    ringbuf: RingBuffer,
}

impl<'a> BpfcontainContext<'a> {
    /// Open, load, and attach BPF objects, then return a new `BpfcontainContext`.
    pub fn new() -> Result<Self> {
        log::debug!("Initializing BPF objects...");

        let mut builder = BpfcontainSkelBuilder::default();
        if log::log_enabled!(log::Level::Trace) {
            builder.obj_builder.debug(true);
        }

        log::debug!("Bumping memlock...");
        bump_memlock_rlimit().context("Failed bumping memlock limit")?;

        log::debug!("Opening eBPF objects...");
        let mut open_skel = builder.open().context("Failed to open skeleton")?;

        initialize_bpf_globals(&mut open_skel).context("Failed to initialize BPF globals")?;

        log::debug!("Loading eBPF objects into kernel...");
        let mut skel = open_skel.load().context("Failed to load skeleton")?;

        log::debug!("Attaching BPF objects to events...");
        skel.attach().context("Failed to attach BPF programs")?;
        attach_uprobes(&mut skel).context("Failed to attach uprobes")?;

        let ringbuf = configure_ringbuf(&mut skel).context("Failed to configure ringbuf")?;

        Ok(BpfcontainContext { skel, ringbuf })
    }

    /// Main BPFContain work loop
    pub fn work_loop(&self) {
        loop {
            if let Err(e) = self.ringbuf.poll(Duration::new(1, 0)) {
                log::warn!("Failed to poll ring buffer: {}", e);
            }
            sleep(Duration::from_millis(100));
        }
    }

    /// Load a policy object into the kernel
    pub fn load_policy(&mut self, policy: &Policy) -> Result<()> {
        log::debug!("Loading policy {}...", policy.name);

        policy
            .load(&mut self.skel)
            .context(format!("Failed to load policy {}", policy.name))
    }

    /// Load policy from a file
    pub fn load_policy_from_file<P: AsRef<Path>>(&mut self, policy_path: P) -> Result<()> {
        log::debug!(
            "Loading policy from file {}...",
            policy_path.as_ref().display()
        );

        let policy = Policy::from_path(&policy_path)?;
        self.load_policy(&policy)?;

        Ok(())
    }

    /// Load policy recursively from a directory
    pub fn load_policy_from_dir<P: AsRef<Path>>(&mut self, policy_dir: P) -> Result<()> {
        log::info!(
            "Loading policy recursively from {}...",
            policy_dir.as_ref().display()
        );

        // Use glob to match all YAML files in the policy directory tree
        for path in glob(&format!("{}/**/*.yml", policy_dir.as_ref().display()))
            .context("Failed to glob policy directory")?
            .filter_map(Result::ok)
        {
            if let Err(e) = self.load_policy_from_file(path) {
                log::warn!("{}", e);
            }
        }

        log::info!("Done loading policy!");

        Ok(())
    }

    /// Unload a policy from the kernel
    pub fn unload_policy(&mut self, policy: &Policy) -> Result<()> {
        log::debug!("Unloading policy {}...", policy.name);

        policy
            .unload(&mut self.skel)
            .context(format!("Failed to unload policy {}", policy.name))
    }
}

/// Set BPF global variables
fn initialize_bpf_globals(open_skel: &mut OpenBpfcontainSkel) -> Result<()> {
    // Set own PID
    open_skel.rodata().bpfcontain_pid = std::process::id();
    // Set own mount ns id
    open_skel.rodata().host_mnt_ns_id = ns::get_current_ns_id(ns::Namespace::Mnt)?;
    // Set own pid ns id
    open_skel.rodata().host_pid_ns_id = ns::get_current_ns_id(ns::Namespace::Pid)?;

    Ok(())
}

/// Attach uprobes to events
fn attach_uprobes(skel: &mut BpfcontainSkel) -> Result<()> {
    // do_containerize
    skel.links.do_containerize = skel
        .progs()
        .do_containerize()
        .attach_uprobe_addr(false, -1, do_containerize as *const () as usize)?
        .into();

    Ok(())
}

/// Configure ring buffers for logging
fn configure_ringbuf(skel: &mut BpfcontainSkel) -> Result<RingBuffer> {
    let mut ringbuf_builder = RingBufferBuilder::default();

    ringbuf_builder
        .add(skel.maps().audit_file_buf(), ringbuf::audit_file)
        .context("Failed to add ringbuf")?
        .add(skel.maps().audit_cap_buf(), ringbuf::audit_cap)
        .context("Failed to add ringbuf")?
        .add(skel.maps().audit_net_buf(), ringbuf::audit_net)
        .context("Failed to add ringbuf")?
        .add(skel.maps().audit_ipc_buf(), ringbuf::audit_ipc)
        .context("Failed to add ringbuf")?;

    ringbuf_builder.build().context("Failed to create ringbuf")
}

/// Callbacks for ring buffer events.
mod ringbuf {
    use pod::Pod as _;

    use crate::bindings::audit::*;

    /// File audit events
    pub fn audit_file(data: &[u8]) -> i32 {
        let event = AuditFile::from_bytes(data).expect("Failed to copy event");

        log::info!("file {}", event);

        0
    }

    /// Capability audit events
    pub fn audit_cap(data: &[u8]) -> i32 {
        let event = AuditCap::from_bytes(data).expect("Failed to copy event");

        log::info!("capability {}", event);

        0
    }

    /// Network audit events
    pub fn audit_net(data: &[u8]) -> i32 {
        let event = AuditNet::from_bytes(data).expect("Failed to copy event");

        log::info!("network {}", event);

        0
    }

    /// IPC audit events
    pub fn audit_ipc(data: &[u8]) -> i32 {
        let event = AuditIpc::from_bytes(data).expect("Failed to copy event");

        log::info!("ipc {}", event);

        0
    }
}
