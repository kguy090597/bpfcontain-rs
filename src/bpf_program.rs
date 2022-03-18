// SPDX-License-Identifier: GPL-2.0-or-later
//
// BPFContain - Container security with eBPF
// Copyright (C) 2020  William Findlay
//
// Dec. 29, 2020  William Findlay  Created this.

//! Functionality related to BPF programs and maps.

use std::fs;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use object::Object;
use object::ObjectSymbol;

use anyhow::{anyhow, Context, Result};
use glob::glob;
use libbpf_rs::{RingBuffer, RingBufferBuilder};
use log::Level;

use crate::bindings::audit;
use crate::bpf::{BpfcontainSkel, BpfcontainSkelBuilder, OpenBpfcontainSkel};
use crate::config::Settings;
use crate::log::log_error;
use crate::ns;
use crate::policy::Policy;
use crate::uprobe_ext::FindSymbolUprobeExt;
use crate::utils::bump_memlock_rlimit;

// Taken from libbpf-bootstrap rust example tracecon
// https://github.com/libbpf/libbpf-bootstrap/blob/master/examples/rust/tracecon/src/main.rs#L47
// Authored by Magnus Kulke
// You can achieve a similar result for testing using objdump -tT so_path | grep fn_name
// Note get_symbol_address will return the deciaml number and objdump uses hex
fn get_symbol_address(so_path: &str, fn_name: &str) -> Result<usize> {
    let path = Path::new(so_path);
    let buffer = fs::read(path)?;
    let file = object::File::parse(buffer.as_slice())?;

    let mut symbols = file.dynamic_symbols();
    let mut kevinsymbols = file.symbols();
    
    if so_path != "/usr/bin/runc" {
        println!("KEVIN DEBUG: {} SYMBOLS",so_path);
        let kevinsymbol = kevinsymbols
            .find(|kevinsymbol| {
                if let Ok(name) = kevinsymbol.name() {
                    //println!("KEVIN DEBUG 3: {:?}",kevinsymbol.name());
                    if name == fn_name {
                        println!("KEVIN DEBUG KEVINSYMBOL FOUND {}",name);
                    }
                    return name == fn_name;
                }
                false
            })
            .ok_or_else(|| anyhow!("symbol not found 2"))?;
        Ok(kevinsymbol.address() as usize)
    }
    else{
        println!("KEVIN DEBUG: {}",so_path);
        let symbol = symbols
	    .find(|symbol| {
	        if let Ok(name) = symbol.name() {
	            //println!("KEVIN DEBUG 2: {:?}",symbol.name());
	            if name == fn_name {
                        println!("KEVIN DEBUG SYMBOL FOUND {}",name);
                    }
	            return name == fn_name;
	        }
	        false
	    })
	    .ok_or_else(|| anyhow!("symbol not found"))?;
	Ok(symbol.address() as usize)
    }
}

pub struct BpfcontainContext<'a> {
    pub skel: BpfcontainSkel<'a>,
    pub ringbuf: RingBuffer,
}

impl<'a> BpfcontainContext<'a> {
    /// Open, load, and attach BPF objects, then return a new `BpfcontainContext`.
    pub fn new(config: &Settings) -> Result<Self> {
        log::debug!("Initializing BPF objects...");

        let mut builder = BpfcontainSkelBuilder::default();
        if log::log_enabled!(log::Level::Trace) {
            builder.obj_builder.debug(true);
        }

        log::debug!("Bumping memlock...");
        bump_memlock_rlimit().context("Failed bumping memlock limit")?;

        log::debug!("Opening eBPF objects...");
        let mut open_skel = builder.open().context("Failed to open skeleton")?;

        initialize_bpf_globals(&mut open_skel, config)
            .context("Failed to initialize BPF globals")?;

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
        log::info!(
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

        // Use glob to match all YAML/TOML/JSON files in the policy directory tree
        for path in glob(&format!("{}/**/*.yml", policy_dir.as_ref().display()))
            .unwrap()
            .chain(glob(&format!("{}/**/*.yaml", policy_dir.as_ref().display())).unwrap())
            .chain(glob(&format!("{}/**/*.toml", policy_dir.as_ref().display())).unwrap())
            .chain(glob(&format!("{}/**/*.json", policy_dir.as_ref().display())).unwrap())
            .filter_map(Result::ok)
        {
            if let Err(e) = self.load_policy_from_file(path) {
                log_error(e, Some(Level::Warn));
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
fn initialize_bpf_globals(open_skel: &mut OpenBpfcontainSkel, config: &Settings) -> Result<()> {
    // Set own PID
    open_skel.rodata().bpfcontain_pid = std::process::id();
    // Set own mount ns id
    open_skel.rodata().host_mnt_ns_id = ns::get_current_ns_id(ns::Namespace::Mnt)?;
    // Set own pid ns id
    open_skel.rodata().host_pid_ns_id = ns::get_current_ns_id(ns::Namespace::Pid)?;
    // Set audit level
    let audit_level = config
        .bpf
        .audit_level
        .iter()
        .map(|x| audit::AuditLevel::from(x.clone()))
        .reduce(|a, b| a | b);
    open_skel.rodata().audit_level = match audit_level {
        Some(level) => level.0,
        None => audit::AuditLevel::AUDIT__NONE.0,
    };

    Ok(())
}

/// Attach uprobes to events
fn attach_uprobes(skel: &mut BpfcontainSkel) -> Result<()> {
    // do_containerize
    skel.links.do_containerize = skel
        .progs_mut()
        .do_containerize()
        .attach_uprobe_addr(
            false,
            -1,
            bpfcontain_uprobes::do_containerize as *const () as usize,
        )?
        .into();

    skel.links.do_apply_policy_to_container = skel
        .progs_mut()
        .do_apply_policy_to_container()
        .attach_uprobe_addr(
            false,
            -1,
            bpfcontain_uprobes::do_apply_policy_to_container as *const () as usize,
        )?
        .into();

    // TODO: Dynamically lookup binary path
    let runc_binary_path = "/usr/bin/runc";
    let runc_func_name = "x_cgo_init";

    let runc_address = get_symbol_address(runc_binary_path, runc_func_name);

    match runc_address {
        Ok(address) => {
            skel.links.runc_x_cgo_init_enter = skel
                .progs_mut()
                .runc_x_cgo_init_enter()
                .attach_uprobe(false, -1, runc_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "Docker support will not work! runc uprobe could not be attached: {}",
                e
            );
        }
    }

    // TODO: Dynamically lookup binary path
    let dockerd_binary_path = "/usr/bin/dockerd";
    let dockerd_func_name = "github.com/docker/docker/container.(*State).SetRunning";

    let dockerd_address = get_symbol_address(dockerd_binary_path, dockerd_func_name);

    match dockerd_address {
        Ok(address) => {
            skel.links.dockerd_container_running_enter = skel
                .progs_mut()
                .dockerd_container_running_enter()
                .attach_uprobe(false, -1, dockerd_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "Docker support will not work! dockerd uprobe could not be attached: {}",
                e
            );
        }
    }

    // CRIO INTEGARTION STUFF BEGINS
    // Change all other uprobes to the handy one-liner
    let crio_binary_path = "/usr/local/bin/crio";
    let crio_main_func = "main.main";

    skel.links.crio_main_enter = skel.progs_mut().crio_main_enter().attach_uprobe_symbol(false,-1,&Path::new(crio_binary_path),crio_main_func)?.into();

    let runc_start_container = "main.startContainer";
    skel.links.runc_start_container_enter = skel.progs_mut().runc_start_container_enter().attach_uprobe_symbol(false,-1,&Path::new(runc_binary_path),runc_start_container)?.into();


    let runc_create_container = "main.createContainer";
    skel.links.runc_create_container_enter = skel.progs_mut().runc_create_container_enter().attach_uprobe_symbol(false,-1,&Path::new(runc_binary_path),runc_create_container)?.into();

    let runc_new_init_process = "github.com/opencontainers/runc/libcontainer.(*linuxContainer).newInitProcess";
    skel.links.runc_init_proc_enter = skel.progs_mut().runc_init_proc_enter().attach_uprobe_symbol(false,-1,&Path::new(runc_binary_path),runc_new_init_process)?.into();

    skel.links.runc_init_proc_start_enter = skel.progs_mut().runc_init_proc_start_enter().attach_uprobe_symbol(false,-1,&Path::new(runc_binary_path),runc_new_init_process)?.into();

    let runc_wait_for_child_exit = "github.com/opencontainers/runc/libcontainer.(*initProcess).waitForChildExit";
    skel.links.runc_wait_for_child_exit_enter = skel.progs_mut().runc_wait_for_child_exit_enter().attach_uprobe_symbol(false,-1,&Path::new(runc_binary_path),runc_wait_for_child_exit)?.into();
    
    let runc_start = "github.com/opencontainers/runc/libcontainer.(*linuxContainer).exec";
    skel.links.runc_start_enter = skel.progs_mut().runc_start_enter().attach_uprobe_symbol(false,-1,&Path::new(runc_binary_path),runc_start)?.into();

    let runc_destroy = "main.destroy";
    skel.links.runc_destroy_enter = skel.progs_mut().runc_destroy_enter().attach_uprobe_symbol(false,-1,&Path::new(runc_binary_path),runc_destroy)?.into();

    /*let crio_binary_path = "/usr/local/bin/crio";
    let crio_func_name9 = "main.main";

    let crio_address9 = get_symbol_address(crio_binary_path, crio_func_name9);

    match crio_address9 {
        Ok(address) => {
            println!("{:x}",address);
            skel.links.crio_main_enter = skel
                .progs_mut()
                .crio_main_enter()
                .attach_uprobe(false, -1, crio_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "CRIO support will not work! crio uprobe could not be attached: {}",
                e
            );
        }
    }*/

    /*// TODO: Dynamically lookup binary path
    let crio_binary_path = "/usr/bin/crio";
    let crio_func1_name = "github.com/cri-o/cri-o/internal/oci.(*Runtime).StartContainer";

    let crio_address1 = get_symbol_address(crio_binary_path, crio_func1_name);

    match crio_address1 {
        Ok(address) => {
            skel.links.crio_container_running_enter1 = skel
                .progs_mut()
                .crio_container_running_enter1()
                .attach_uprobe(false, -1, crio_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "CRIO support will not work! crio uprobe could not be attached: {}",
                e
            );
        }
    }
    
    // TODO: Dynamically lookup binary path
    let crio_func2_name = "github.com/cri-o/cri-o/internal/oci.(*runtimeOCI).StartContainer";

    let crio_address2 = get_symbol_address(crio_binary_path, crio_func2_name);

    match crio_address2 {
        Ok(address) => {
            skel.links.crio_container_running_enter2 = skel
                .progs_mut()
                .crio_container_running_enter2()
                .attach_uprobe(false, -1, crio_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "CRIO support will not work! crio uprobe could not be attached: {}",
                e
            );
        }
    }
    
    // TODO: Dynamically lookup binary path
    let crio_func3_name = "github.com/cri-o/cri-o/internal/oci.(*runtimeVM).StartContainer";

    let crio_address3 = get_symbol_address(crio_binary_path, crio_func3_name);

    match crio_address3 {
        Ok(address) => {
            skel.links.crio_container_running_enter3 = skel
                .progs_mut()
                .crio_container_running_enter3()
                .attach_uprobe(false, -1, crio_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "CRIO support will not work! crio uprobe could not be attached: {}",
                e
            );
        }
    }
    
    // TODO: Dynamically lookup binary path
    let crio_func4_name = "github.com/cri-o/cri-o/internal/storage.(*runtimeService).StartContainer";

    let crio_address4 = get_symbol_address(crio_binary_path, crio_func4_name);

    match crio_address4 {
        Ok(address) => {
            skel.links.crio_container_running_enter4 = skel
                .progs_mut()
                .crio_container_running_enter4()
                .attach_uprobe(false, -1, crio_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "CRIO support will not work! crio uprobe could not be attached: {}",
                e
            );
        }
    }
    
    // TODO: Dynamically lookup binary path
    let crio_func5_name = "/usr/bin/crio:github.com/cri-o/cri-o/server.(*Server).StartContainer";

    let crio_address5 = get_symbol_address(crio_binary_path, crio_func5_name);

    match crio_address5 {
        Ok(address) => {
            skel.links.crio_container_running_enter5 = skel
                .progs_mut()
                .crio_container_running_enter5()
                .attach_uprobe(false, -1, crio_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "CRIO support will not work! crio uprobe could not be attached: {}",
                e
            );
        }
    }
    
    // TODO: Dynamically lookup binary path
    let crio_func6_name = "github.com/cri-o/cri-o/server/cri/v1.(*service).StartContainer";

    let crio_address6 = get_symbol_address(crio_binary_path, crio_func6_name);

    match crio_address6 {
        Ok(address) => {
            skel.links.crio_container_running_enter6 = skel
                .progs_mut()
                .crio_container_running_enter6()
                .attach_uprobe(false, -1, crio_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "CRIO support will not work! crio uprobe could not be attached: {}",
                e
            );
        }
    }
    
    // TODO: Dynamically lookup binary path
    let crio_func7_name = "github.com/cri-o/cri-o/server/cri/v1alpha2.(*service).StartContainer";

    let crio_address7 = get_symbol_address(crio_binary_path, crio_func7_name);

    match crio_address7 {
        Ok(address) => {
            skel.links.crio_container_running_enter7 = skel
                .progs_mut()
                .crio_container_running_enter7()
                .attach_uprobe(false, -1, crio_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "CRIO support will not work! crio uprobe could not be attached: {}",
                e
            );
        }
    }

    // TODO: Dynamically lookup binary path
    let crio_func8_name = "k8s.io/cri-api/pkg/apis/runtime/v1.xxx_messageInfo_StartContainerRequest";

    let crio_address8 = get_symbol_address(crio_binary_path, crio_func8_name);

    match crio_address8 {
        Ok(address) => {
            skel.links.crio_container_running_enter8 = skel
                .progs_mut()
                .crio_container_running_enter8()
                .attach_uprobe(false, -1, crio_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "CRIO support will not work! crio uprobe could not be attached: {}",
                e
            );
        }
    }

    // TODO: Dynamically lookup binary path
    let crictl_binary_path = "/home/kevin/Desktop/cri-tools/build/bin/crictl";

    let crictl_func_name = "main.StartContainer";

    let crictl_address = get_symbol_address(crictl_binary_path, crictl_func_name);

    match crictl_address {
        Ok(address) => {
            skel.links.crictl_container_running_enter = skel
                .progs_mut()
                .crictl_container_running_enter()
                .attach_uprobe(false, -1, crictl_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "CRICTL support will not work! crio uprobe could not be attached: {}",
                e
            );
        }
    }

    let crictl_func_name2 = "main.main";

    let crictl_address2 = get_symbol_address(crictl_binary_path, crictl_func_name2);

    match crictl_address2 {
        Ok(address) => {
            skel.links.crictl_main_enter = skel
                .progs_mut()
                .crictl_main_enter()
                .attach_uprobe(false, -1, crictl_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "CRICTL support will not work! crio uprobe could not be attached: {}",
                e
            );
        }
    }

    let crio_func_name9 = "main.main";

    let crio_address9 = get_symbol_address(crictl_binary_path, crio_func_name9);

    match crio_address9 {
        Ok(address) => {
            skel.links.crio_main_enter = skel
                .progs_mut()
                .crio_main_enter()
                .attach_uprobe(false, -1, crio_binary_path, address)?
                .into();
        }
        Err(e) => {
            log::warn!(
                "CRICTL support will not work! crio uprobe could not be attached: {}",
                e
            );
        }
    }*/

    Ok(())
}

/// Configure ring buffers for logging
fn configure_ringbuf(skel: &mut BpfcontainSkel) -> Result<RingBuffer> {
    let mut ringbuf_builder = RingBufferBuilder::default();

    ringbuf_builder
        .add(skel.maps().__audit_buf(), audit::audit_callback)
        .context("Failed to add callback")?;

    ringbuf_builder.build().context("Failed to create ringbuf")
}
