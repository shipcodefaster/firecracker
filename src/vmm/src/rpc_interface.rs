// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter};
use std::result;
use std::sync::{Arc, Mutex};

use super::Error as VmmError;
#[cfg(not(test))]
use super::{
    builder::build_microvm_for_boot, persist::create_snapshot, persist::restore_from_snapshot,
    resources::VmResources, Vmm,
};
use crate::persist::{CreateSnapshotError, LoadSnapshotError};
use crate::resources::VmmConfig;
use crate::version_map::VERSION_MAP;
use crate::vmm_config::balloon::{
    BalloonConfigError, BalloonDeviceConfig, BalloonStats, BalloonUpdateConfig,
    BalloonUpdateStatsConfig,
};
use crate::vmm_config::boot_source::{BootSourceConfig, BootSourceConfigError};
use crate::vmm_config::drive::{BlockDeviceConfig, BlockDeviceUpdateConfig, DriveError};
use crate::vmm_config::instance_info::InstanceInfo;
use crate::vmm_config::logger::{LoggerConfig, LoggerConfigError};
use crate::vmm_config::machine_config::{VmConfig, VmConfigError};
use crate::vmm_config::metrics::{MetricsConfig, MetricsConfigError};
use crate::vmm_config::mmds::{MmdsConfig, MmdsConfigError};
use crate::vmm_config::net::{
    NetworkInterfaceConfig, NetworkInterfaceError, NetworkInterfaceUpdateConfig,
};
use crate::vmm_config::snapshot::{CreateSnapshotParams, LoadSnapshotParams, SnapshotType};
use crate::vmm_config::vsock::{VsockConfigError, VsockDeviceConfig};
use crate::vmm_config::{self, RateLimiterUpdate};
use crate::{builder::StartMicrovmError, EventManager};
use crate::{ExitCode, FC_EXIT_CODE_BAD_CONFIGURATION};
use logger::{info, update_metric_with_elapsed_time, METRICS};
use seccompiler::BpfThreadMap;
#[cfg(test)]
use tests::{
    build_microvm_for_boot, create_snapshot, restore_from_snapshot, MockVmRes as VmResources,
    MockVmm as Vmm,
};

/// This enum represents the public interface of the VMM. Each action contains various
/// bits of information (ids, paths, etc.).
#[derive(PartialEq)]
pub enum VmmAction {
    /// Configure the boot source of the microVM using as input the `ConfigureBootSource`. This
    /// action can only be called before the microVM has booted.
    ConfigureBootSource(BootSourceConfig),
    /// Configure the logger using as input the `LoggerConfig`. This action can only be called
    /// before the microVM has booted.
    ConfigureLogger(LoggerConfig),
    /// Configure the metrics using as input the `MetricsConfig`. This action can only be called
    /// before the microVM has booted.
    ConfigureMetrics(MetricsConfig),
    /// Create a snapshot using as input the `CreateSnapshotParams`. This action can only be called
    /// after the microVM has booted and only when the microVM is in `Paused` state.
    CreateSnapshot(CreateSnapshotParams),
    /// Get the balloon device configuration.
    GetBalloonConfig,
    /// Get the ballon device latest statistics.
    GetBalloonStats,
    /// Get complete microVM configuration in JSON format.
    GetFullVmConfig,
    /// Get the machine configuration of the microVM.
    GetVmMachineConfig,
    /// Get microVM instance information.
    GetVmInstanceInfo,
    /// Get microVM version.
    GetVmmVersion,
    /// Flush the metrics. This action can only be called after the logger has been configured.
    FlushMetrics,
    /// Add a new block device or update one that already exists using the `BlockDeviceConfig` as
    /// input. This action can only be called before the microVM has booted.
    InsertBlockDevice(BlockDeviceConfig),
    /// Add a new network interface config or update one that already exists using the
    /// `NetworkInterfaceConfig` as input. This action can only be called before the microVM has
    /// booted.
    InsertNetworkDevice(NetworkInterfaceConfig),
    /// Load the microVM state using as input the `LoadSnapshotParams`. This action can only be
    /// called before the microVM has booted. If this action is successful, the loaded microVM will
    /// be in `Paused` state. Should change this state to `Resumed` for the microVM to run.
    LoadSnapshot(LoadSnapshotParams),
    /// Pause the guest, by pausing the microVM VCPUs.
    Pause,
    /// Resume the guest, by resuming the microVM VCPUs.
    Resume,
    /// Set the balloon device or update the one that already exists using the
    /// `BalloonDeviceConfig` as input. This action can only be called before the microVM
    /// has booted.
    SetBalloonDevice(BalloonDeviceConfig),
    /// Set the MMDS configuration.
    SetMmdsConfiguration(MmdsConfig),
    /// Set the vsock device or update the one that already exists using the
    /// `VsockDeviceConfig` as input. This action can only be called before the microVM has
    /// booted.
    SetVsockDevice(VsockDeviceConfig),
    /// Set the microVM configuration (memory & vcpu) using `VmConfig` as input. This
    /// action can only be called before the microVM has booted.
    SetVmConfiguration(VmConfig),
    /// Launch the microVM. This action can only be called before the microVM has booted.
    StartMicroVm,
    /// Send CTRL+ALT+DEL to the microVM, using the i8042 keyboard function. If an AT-keyboard
    /// driver is listening on the guest end, this can be used to shut down the microVM gracefully.
    #[cfg(target_arch = "x86_64")]
    SendCtrlAltDel,
    /// Update the balloon size, after microVM start.
    UpdateBalloon(BalloonUpdateConfig),
    /// Update the balloon statistics polling interval, after microVM start.
    UpdateBalloonStatistics(BalloonUpdateStatsConfig),
    /// Update existing block device properties such as `path_on_host` or `rate_limiter`.
    UpdateBlockDevice(BlockDeviceUpdateConfig),
    /// Update a network interface, after microVM start. Currently, the only updatable properties
    /// are the RX and TX rate limiters.
    UpdateNetworkInterface(NetworkInterfaceUpdateConfig),
}

/// Wrapper for all errors associated with VMM actions.
#[derive(Debug)]
pub enum VmmActionError {
    /// The action `SetBalloonDevice` failed because of bad user input.
    BalloonConfig(BalloonConfigError),
    /// The action `ConfigureBootSource` failed because of bad user input.
    BootSource(BootSourceConfigError),
    /// The action `CreateSnapshot` failed.
    CreateSnapshot(CreateSnapshotError),
    /// One of the actions `InsertBlockDevice` or `UpdateBlockDevicePath`
    /// failed because of bad user input.
    DriveConfig(DriveError),
    /// Internal Vmm error.
    InternalVmm(VmmError),
    /// Loading a microVM snapshot failed.
    LoadSnapshot(LoadSnapshotError),
    /// Loading a microVM snapshot not allowed after configuring boot-specific resources.
    LoadSnapshotNotAllowed,
    /// The action `ConfigureLogger` failed because of bad user input.
    Logger(LoggerConfigError),
    /// One of the actions `GetVmConfiguration` or `SetVmConfiguration` failed because of bad input.
    MachineConfig(VmConfigError),
    /// The action `ConfigureMetrics` failed because of bad user input.
    Metrics(MetricsConfigError),
    /// The action `SetMmdsConfiguration` failed because of bad user input.
    MmdsConfig(MmdsConfigError),
    /// The action `InsertNetworkDevice` failed because of bad user input.
    NetworkConfig(NetworkInterfaceError),
    /// The requested operation is not supported.
    NotSupported(String),
    /// The requested operation is not supported after starting the microVM.
    OperationNotSupportedPostBoot,
    /// The requested operation is not supported before starting the microVM.
    OperationNotSupportedPreBoot,
    /// The action `StartMicroVm` failed because of an internal error.
    StartMicrovm(StartMicrovmError),
    /// The action `SetVsockDevice` failed because of bad user input.
    VsockConfig(VsockConfigError),
}

impl Display for VmmActionError {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::VmmActionError::*;

        write!(
            f,
            "{}",
            match self {
                BalloonConfig(err) => err.to_string(),
                BootSource(err) => err.to_string(),
                CreateSnapshot(err) => err.to_string(),
                DriveConfig(err) => err.to_string(),
                InternalVmm(err) => format!("Internal Vmm error: {}", err),
                LoadSnapshot(err) => format!("Load microVM snapshot error: {}", err),
                LoadSnapshotNotAllowed => {
                    "Loading a microVM snapshot not allowed after configuring boot-specific resources."
                        .to_string()
                }
                Logger(err) => err.to_string(),
                MachineConfig(err) => err.to_string(),
                Metrics(err) => err.to_string(),
                MmdsConfig(err) => err.to_string(),
                NetworkConfig(err) => err.to_string(),
                NotSupported(err) => format!("The requested operation is not supported: {}", err),
                OperationNotSupportedPostBoot => {
                    "The requested operation is not supported after starting the microVM."
                        .to_string()
                }
                OperationNotSupportedPreBoot => {
                    "The requested operation is not supported before starting the microVM."
                        .to_string()
                }
                StartMicrovm(err) => err.to_string(),
                // The action `SetVsockDevice` failed because of bad user input.
                VsockConfig(err) => err.to_string(),
            }
        )
    }
}

/// The enum represents the response sent by the VMM in case of success. The response is either
/// empty, when no data needs to be sent, or an internal VMM structure.
#[derive(Debug, PartialEq)]
pub enum VmmData {
    /// The balloon device configuration.
    BalloonConfig(BalloonDeviceConfig),
    /// The latest balloon device statistics.
    BalloonStats(BalloonStats),
    /// No data is sent on the channel.
    Empty,
    /// The complete microVM configuration in JSON format.
    FullVmConfig(VmmConfig),
    /// The microVM configuration represented by `VmConfig`.
    MachineConfiguration(VmConfig),
    /// The microVM instance information.
    InstanceInformation(InstanceInfo),
    /// The microVM version.
    VmmVersion(String),
}

/// Shorthand result type for external VMM commands.
pub type ActionResult = result::Result<VmmData, VmmActionError>;

/// Enables pre-boot setup and instantiation of a Firecracker VMM.
pub struct PrebootApiController<'a> {
    seccomp_filters: &'a BpfThreadMap,
    instance_info: InstanceInfo,
    vm_resources: &'a mut VmResources,
    event_manager: &'a mut EventManager,
    built_vmm: Option<Arc<Mutex<Vmm>>>,
    // Configuring boot specific resources will set this to true.
    // Loading from snapshot will not be allowed once this is true.
    boot_path: bool,
    // Some PrebootApiRequest errors are irrecoverable and Firecracker
    // should cleanly teardown if they occur.
    fatal_error: Option<ExitCode>,
}

impl<'a> PrebootApiController<'a> {
    /// Constructor for the PrebootApiController.
    pub fn new(
        seccomp_filters: &'a BpfThreadMap,
        instance_info: InstanceInfo,
        vm_resources: &'a mut VmResources,
        event_manager: &'a mut EventManager,
    ) -> PrebootApiController<'a> {
        PrebootApiController {
            seccomp_filters,
            instance_info,
            vm_resources,
            event_manager,
            built_vmm: None,
            boot_path: false,
            fatal_error: None,
        }
    }

    /// Default implementation for the function that builds and starts a microVM.
    /// It takes two closures `recv_req` and `respond` as params which abstract away
    /// the message transport.
    ///
    /// Returns a populated `VmResources` object and a running `Vmm` object.
    pub fn build_microvm_from_requests<F, G>(
        seccomp_filters: &BpfThreadMap,
        event_manager: &mut EventManager,
        instance_info: InstanceInfo,
        recv_req: F,
        respond: G,
        boot_timer_enabled: bool,
    ) -> result::Result<(VmResources, Arc<Mutex<Vmm>>), ExitCode>
    where
        F: Fn() -> VmmAction,
        G: Fn(ActionResult),
    {
        let mut vm_resources = VmResources::default();
        // Silence false clippy warning. Clippy suggests using
        // VmResources { boot_timer: boot_timer_enabled, ..Default::default() }; but this will
        // generate build errors because VmResources contains private fields.
        #[allow(clippy::field_reassign_with_default)]
        {
            vm_resources.boot_timer = boot_timer_enabled;
        }
        let mut preboot_controller = PrebootApiController::new(
            seccomp_filters,
            instance_info,
            &mut vm_resources,
            event_manager,
        );
        // Configure and start microVM through successive API calls.
        // Iterate through API calls to configure microVm.
        // The loop breaks when a microVM is successfully started, and a running Vmm is built.
        while preboot_controller.built_vmm.is_none() {
            // Get request, process it, send back the response.
            respond(preboot_controller.handle_preboot_request(recv_req()));
            // If any fatal errors were encountered, break the loop.
            if let Some(exit_code) = preboot_controller.fatal_error {
                return Err(exit_code);
            }
        }

        // Safe to unwrap because previous loop cannot end on None.
        let vmm = preboot_controller.built_vmm.unwrap();
        Ok((vm_resources, vmm))
    }

    /// Handles the incoming preboot request and provides a response for it.
    /// Returns a built/running `Vmm` after handling a successful `StartMicroVm` request.
    pub fn handle_preboot_request(&mut self, request: VmmAction) -> ActionResult {
        use self::VmmAction::*;

        match request {
            // Supported operations allowed pre-boot.
            ConfigureBootSource(config) => self.set_boot_source(config),
            ConfigureLogger(logger_cfg) => {
                vmm_config::logger::init_logger(logger_cfg, &self.instance_info)
                    .map(|()| VmmData::Empty)
                    .map_err(VmmActionError::Logger)
            }
            ConfigureMetrics(metrics_cfg) => vmm_config::metrics::init_metrics(metrics_cfg)
                .map(|()| VmmData::Empty)
                .map_err(VmmActionError::Metrics),
            GetBalloonConfig => self.balloon_config(),
            GetFullVmConfig => Ok(VmmData::FullVmConfig((&*self.vm_resources).into())),
            GetVmMachineConfig => Ok(VmmData::MachineConfiguration(
                self.vm_resources.vm_config().clone(),
            )),
            GetVmInstanceInfo => Ok(VmmData::InstanceInformation(self.instance_info.clone())),
            GetVmmVersion => Ok(VmmData::VmmVersion(self.instance_info.vmm_version.clone())),
            InsertBlockDevice(config) => self.insert_block_device(config),
            InsertNetworkDevice(config) => self.insert_net_device(config),
            LoadSnapshot(config) => self.load_snapshot(&config),
            SetBalloonDevice(config) => self.set_balloon_device(config),
            SetVsockDevice(config) => self.set_vsock_device(config),
            SetVmConfiguration(config) => self.set_vm_config(config),
            SetMmdsConfiguration(config) => self.set_mmds_config(config),
            StartMicroVm => self.start_microvm(),
            // Operations not allowed pre-boot.
            CreateSnapshot(_)
            | FlushMetrics
            | Pause
            | Resume
            | GetBalloonStats
            | UpdateBalloon(_)
            | UpdateBalloonStatistics(_)
            | UpdateBlockDevice(_)
            | UpdateNetworkInterface(_) => Err(VmmActionError::OperationNotSupportedPreBoot),
            #[cfg(target_arch = "x86_64")]
            SendCtrlAltDel => Err(VmmActionError::OperationNotSupportedPreBoot),
        }
    }

    fn balloon_config(&mut self) -> ActionResult {
        self.vm_resources
            .balloon
            .get_config()
            .map(VmmData::BalloonConfig)
            .map_err(VmmActionError::BalloonConfig)
    }

    fn insert_block_device(&mut self, cfg: BlockDeviceConfig) -> ActionResult {
        self.boot_path = true;
        self.vm_resources
            .set_block_device(cfg)
            .map(|()| VmmData::Empty)
            .map_err(VmmActionError::DriveConfig)
    }

    fn insert_net_device(&mut self, cfg: NetworkInterfaceConfig) -> ActionResult {
        self.boot_path = true;
        self.vm_resources
            .build_net_device(cfg)
            .map(|()| VmmData::Empty)
            .map_err(VmmActionError::NetworkConfig)
    }

    fn set_balloon_device(&mut self, cfg: BalloonDeviceConfig) -> ActionResult {
        self.boot_path = true;
        self.vm_resources
            .set_balloon_device(cfg)
            .map(|()| VmmData::Empty)
            .map_err(VmmActionError::BalloonConfig)
    }

    fn set_boot_source(&mut self, cfg: BootSourceConfig) -> ActionResult {
        self.boot_path = true;
        self.vm_resources
            .set_boot_source(cfg)
            .map(|()| VmmData::Empty)
            .map_err(VmmActionError::BootSource)
    }

    fn set_mmds_config(&mut self, cfg: MmdsConfig) -> ActionResult {
        self.boot_path = true;
        self.vm_resources
            .set_mmds_config(cfg)
            .map(|()| VmmData::Empty)
            .map_err(VmmActionError::MmdsConfig)
    }

    fn set_vm_config(&mut self, cfg: VmConfig) -> ActionResult {
        self.boot_path = true;
        self.vm_resources
            .set_vm_config(&cfg)
            .map(|()| VmmData::Empty)
            .map_err(VmmActionError::MachineConfig)
    }

    fn set_vsock_device(&mut self, cfg: VsockDeviceConfig) -> ActionResult {
        self.boot_path = true;
        self.vm_resources
            .set_vsock_device(cfg)
            .map(|()| VmmData::Empty)
            .map_err(VmmActionError::VsockConfig)
    }

    // On success, this command will end the pre-boot stage and this controller
    // will be replaced by a runtime controller.
    fn start_microvm(&mut self) -> ActionResult {
        build_microvm_for_boot(
            &self.instance_info,
            &self.vm_resources,
            &mut self.event_manager,
            self.seccomp_filters,
        )
        .map(|vmm| {
            self.built_vmm = Some(vmm);
            VmmData::Empty
        })
        .map_err(VmmActionError::StartMicrovm)
    }

    // On success, this command will end the pre-boot stage and this controller
    // will be replaced by a runtime controller.
    fn load_snapshot(&mut self, load_params: &LoadSnapshotParams) -> ActionResult {
        let load_start_us = utils::time::get_time_us(utils::time::ClockType::Monotonic);

        if self.boot_path {
            let err = VmmActionError::LoadSnapshotNotAllowed;
            info!("{}", err);
            return Err(err);
        }

        if load_params.enable_diff_snapshots {
            self.vm_resources.set_track_dirty_pages(true);
        }

        let result = restore_from_snapshot(
            &self.instance_info,
            &mut self.event_manager,
            self.seccomp_filters,
            load_params,
            VERSION_MAP.clone(),
        )
        .and_then(|vmm| {
            let ret = if load_params.resume_vm {
                vmm.lock().expect("Poisoned lock").resume_vm()
            } else {
                Ok(())
            };
            ret.map(|()| {
                self.built_vmm = Some(vmm);
                VmmData::Empty
            })
            .map_err(LoadSnapshotError::ResumeMicroVm)
        })
        .map_err(|e| {
            // The process is too dirty to recover at this point.
            self.fatal_error = Some(FC_EXIT_CODE_BAD_CONFIGURATION);
            VmmActionError::LoadSnapshot(e)
        });

        let elapsed_time_us =
            update_metric_with_elapsed_time(&METRICS.latencies_us.vmm_load_snapshot, load_start_us);
        info!("'load snapshot' VMM action took {} us.", elapsed_time_us);

        result
    }
}

/// Enables RPC interaction with a running Firecracker VMM.
pub struct RuntimeApiController {
    vmm: Arc<Mutex<Vmm>>,
    vm_resources: VmResources,
}

impl RuntimeApiController {
    /// Handles the incoming runtime `VmmAction` request and provides a response for it.
    pub fn handle_request(&mut self, request: VmmAction) -> ActionResult {
        use self::VmmAction::*;
        match request {
            // Supported operations allowed post-boot.
            CreateSnapshot(snapshot_create_cfg) => self.create_snapshot(&snapshot_create_cfg),
            FlushMetrics => self.flush_metrics(),
            GetBalloonConfig => self
                .vmm
                .lock()
                .expect("Poisoned lock")
                .balloon_config()
                .map(|state| VmmData::BalloonConfig(BalloonDeviceConfig::from(state)))
                .map_err(|e| VmmActionError::BalloonConfig(BalloonConfigError::from(e))),
            GetBalloonStats => self
                .vmm
                .lock()
                .expect("Poisoned lock")
                .latest_balloon_stats()
                .map(VmmData::BalloonStats)
                .map_err(|e| VmmActionError::BalloonConfig(BalloonConfigError::from(e))),
            GetFullVmConfig => Ok(VmmData::FullVmConfig((&self.vm_resources).into())),
            GetVmMachineConfig => Ok(VmmData::MachineConfiguration(
                self.vm_resources.vm_config().clone(),
            )),
            GetVmInstanceInfo => Ok(VmmData::InstanceInformation(
                self.vmm.lock().expect("Poisoned lock").instance_info(),
            )),
            GetVmmVersion => Ok(VmmData::VmmVersion(
                self.vmm.lock().expect("Poisoned lock").version(),
            )),
            Pause => self.pause(),
            Resume => self.resume(),
            #[cfg(target_arch = "x86_64")]
            SendCtrlAltDel => self.send_ctrl_alt_del(),
            UpdateBalloon(balloon_update) => self
                .vmm
                .lock()
                .expect("Poisoned lock")
                .update_balloon_config(balloon_update.amount_mib)
                .map(|_| VmmData::Empty)
                .map_err(|e| VmmActionError::BalloonConfig(BalloonConfigError::from(e))),
            UpdateBalloonStatistics(balloon_stats_update) => self
                .vmm
                .lock()
                .expect("Poisoned lock")
                .update_balloon_stats_config(balloon_stats_update.stats_polling_interval_s)
                .map(|_| VmmData::Empty)
                .map_err(|e| VmmActionError::BalloonConfig(BalloonConfigError::from(e))),
            UpdateBlockDevice(new_cfg) => self.update_block_device(new_cfg),
            UpdateNetworkInterface(netif_update) => self.update_net_rate_limiters(netif_update),

            // Operations not allowed post-boot.
            ConfigureBootSource(_)
            | ConfigureLogger(_)
            | ConfigureMetrics(_)
            | InsertBlockDevice(_)
            | InsertNetworkDevice(_)
            | LoadSnapshot(_)
            | SetBalloonDevice(_)
            | SetVsockDevice(_)
            | SetMmdsConfiguration(_)
            | SetVmConfiguration(_)
            | StartMicroVm => Err(VmmActionError::OperationNotSupportedPostBoot),
        }
    }

    /// Creates a new `RuntimeApiController`.
    pub fn new(vm_resources: VmResources, vmm: Arc<Mutex<Vmm>>) -> Self {
        Self { vmm, vm_resources }
    }

    /// Pauses the microVM by pausing the vCPUs.
    pub fn pause(&mut self) -> ActionResult {
        let pause_start_us = utils::time::get_time_us(utils::time::ClockType::Monotonic);

        self.vmm
            .lock()
            .expect("Poisoned lock")
            .pause_vm()
            .map_err(VmmActionError::InternalVmm)?;

        let elapsed_time_us =
            update_metric_with_elapsed_time(&METRICS.latencies_us.vmm_pause_vm, pause_start_us);
        info!("'pause vm' VMM action took {} us.", elapsed_time_us);

        Ok(VmmData::Empty)
    }

    /// Resumes the microVM by resuming the vCPUs.
    pub fn resume(&mut self) -> ActionResult {
        let resume_start_us = utils::time::get_time_us(utils::time::ClockType::Monotonic);

        self.vmm
            .lock()
            .expect("Poisoned lock")
            .resume_vm()
            .map_err(VmmActionError::InternalVmm)?;

        let elapsed_time_us =
            update_metric_with_elapsed_time(&METRICS.latencies_us.vmm_resume_vm, resume_start_us);
        info!("'resume vm' VMM action took {} us.", elapsed_time_us);

        Ok(VmmData::Empty)
    }

    /// Write the metrics on user demand (flush). We use the word `flush` here to highlight the fact
    /// that the metrics will be written immediately.
    /// Defer to inner Vmm. We'll move to a variant where the Vmm simply exposes functionality like
    /// getting the dirty pages, and then we'll have the metrics flushing logic entirely on the outside.
    fn flush_metrics(&mut self) -> ActionResult {
        // FIXME: we're losing the bool saying whether metrics were actually written.
        METRICS
            .write()
            .map(|_| VmmData::Empty)
            .map_err(super::Error::Metrics)
            .map_err(VmmActionError::InternalVmm)
    }

    /// Injects CTRL+ALT+DEL keystroke combo to the inner Vmm (if present).
    #[cfg(target_arch = "x86_64")]
    fn send_ctrl_alt_del(&mut self) -> ActionResult {
        self.vmm
            .lock()
            .expect("Poisoned lock")
            .send_ctrl_alt_del()
            .map(|()| VmmData::Empty)
            .map_err(VmmActionError::InternalVmm)
    }

    fn create_snapshot(&mut self, create_params: &CreateSnapshotParams) -> ActionResult {
        if create_params.snapshot_type == SnapshotType::Diff
            && !self.vm_resources.track_dirty_pages()
        {
            return Err(VmmActionError::NotSupported(
                "Diff snapshots are not allowed on uVMs with dirty page tracking disabled."
                    .to_string(),
            ));
        }

        let mut locked_vmm = self.vmm.lock().unwrap();
        let create_start_us = utils::time::get_time_us(utils::time::ClockType::Monotonic);

        create_snapshot(&mut locked_vmm, create_params, VERSION_MAP.clone())
            .map_err(VmmActionError::CreateSnapshot)?;

        match create_params.snapshot_type {
            SnapshotType::Full => {
                let elapsed_time_us = update_metric_with_elapsed_time(
                    &METRICS.latencies_us.vmm_full_create_snapshot,
                    create_start_us,
                );
                info!(
                    "'create full snapshot' VMM action took {} us.",
                    elapsed_time_us
                );
            }
            SnapshotType::Diff => {
                let elapsed_time_us = update_metric_with_elapsed_time(
                    &METRICS.latencies_us.vmm_diff_create_snapshot,
                    create_start_us,
                );
                info!(
                    "'create diff snapshot' VMM action took {} us.",
                    elapsed_time_us
                );
            }
        }
        Ok(VmmData::Empty)
    }

    /// Updates block device properties:
    ///  - path of the host file backing the emulated block device,
    ///    update the disk image on the device and its virtio configuration
    ///  - rate limiter configuration.
    fn update_block_device(&mut self, new_cfg: BlockDeviceUpdateConfig) -> ActionResult {
        let mut vmm = self.vmm.lock().expect("Poisoned lock");
        if let Some(new_path) = new_cfg.path_on_host {
            vmm.update_block_device_path(&new_cfg.drive_id, new_path)
                .map(|()| VmmData::Empty)
                .map_err(DriveError::DeviceUpdate)
                .map_err(VmmActionError::DriveConfig)?;
        }
        if new_cfg.rate_limiter.is_some() {
            vmm.update_block_rate_limiter(
                &new_cfg.drive_id,
                RateLimiterUpdate::from(new_cfg.rate_limiter).bandwidth,
                RateLimiterUpdate::from(new_cfg.rate_limiter).ops,
            )
            .map(|()| VmmData::Empty)
            .map_err(DriveError::DeviceUpdate)
            .map_err(VmmActionError::DriveConfig)?;
        }
        Ok(VmmData::Empty)
    }

    /// Updates configuration for an emulated net device as described in `new_cfg`.
    fn update_net_rate_limiters(&mut self, new_cfg: NetworkInterfaceUpdateConfig) -> ActionResult {
        self.vmm
            .lock()
            .expect("Poisoned lock")
            .update_net_rate_limiters(
                &new_cfg.iface_id,
                RateLimiterUpdate::from(new_cfg.rx_rate_limiter).bandwidth,
                RateLimiterUpdate::from(new_cfg.rx_rate_limiter).ops,
                RateLimiterUpdate::from(new_cfg.tx_rate_limiter).bandwidth,
                RateLimiterUpdate::from(new_cfg.tx_rate_limiter).ops,
            )
            .map(|()| VmmData::Empty)
            .map_err(NetworkInterfaceError::DeviceUpdate)
            .map_err(VmmActionError::NetworkConfig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmm_config::balloon::BalloonBuilder;
    use crate::vmm_config::drive::CacheType;
    use crate::vmm_config::logger::LoggerLevel;
    use crate::vmm_config::vsock::VsockBuilder;
    use devices::virtio::balloon::{BalloonConfig, Error as BalloonError};
    use devices::virtio::VsockError;
    use seccompiler::BpfThreadMap;

    use std::path::PathBuf;

    impl PartialEq for VmmActionError {
        fn eq(&self, other: &VmmActionError) -> bool {
            use VmmActionError::*;
            matches!(
                (self, other),
                (BalloonConfig(_), BalloonConfig(_))
                    | (BootSource(_), BootSource(_))
                    | (CreateSnapshot(_), CreateSnapshot(_))
                    | (DriveConfig(_), DriveConfig(_))
                    | (InternalVmm(_), InternalVmm(_))
                    | (LoadSnapshot(_), LoadSnapshot(_))
                    | (LoadSnapshotNotAllowed, LoadSnapshotNotAllowed)
                    | (Logger(_), Logger(_))
                    | (MachineConfig(_), MachineConfig(_))
                    | (Metrics(_), Metrics(_))
                    | (MmdsConfig(_), MmdsConfig(_))
                    | (NetworkConfig(_), NetworkConfig(_))
                    | (NotSupported(_), NotSupported(_))
                    | (OperationNotSupportedPostBoot, OperationNotSupportedPostBoot)
                    | (OperationNotSupportedPreBoot, OperationNotSupportedPreBoot)
                    | (StartMicrovm(_), StartMicrovm(_))
                    | (VsockConfig(_), VsockConfig(_))
            )
        }
    }

    // Mock `VmResources` used for testing.
    #[derive(Default)]
    pub struct MockVmRes {
        vm_config: VmConfig,
        pub balloon: BalloonBuilder,
        pub vsock: VsockBuilder,
        balloon_config_called: bool,
        balloon_set: bool,
        boot_cfg_set: bool,
        block_set: bool,
        vsock_set: bool,
        net_set: bool,
        mmds_set: bool,
        pub boot_timer: bool,
        // when `true`, all self methods are forced to fail
        pub force_errors: bool,
    }

    impl MockVmRes {
        pub fn vm_config(&self) -> &VmConfig {
            &self.vm_config
        }

        pub fn balloon_config(&mut self) -> Result<BalloonConfig, BalloonError> {
            if self.force_errors {
                return Err(BalloonError::DeviceNotFound);
            }
            self.balloon_config_called = true;
            Ok(BalloonConfig::default())
        }

        pub fn track_dirty_pages(&self) -> bool {
            self.vm_config().track_dirty_pages
        }

        pub fn set_track_dirty_pages(&mut self, dirty_page_tracking: bool) {
            self.vm_config.track_dirty_pages = dirty_page_tracking;
        }

        pub fn set_vm_config(&mut self, machine_config: &VmConfig) -> Result<(), VmConfigError> {
            if self.force_errors {
                return Err(VmConfigError::InvalidVcpuCount);
            }
            self.vm_config = machine_config.clone();
            Ok(())
        }

        pub fn set_balloon_device(
            &mut self,
            _: BalloonDeviceConfig,
        ) -> Result<(), BalloonConfigError> {
            if self.force_errors {
                return Err(BalloonConfigError::DeviceNotFound);
            }
            self.balloon_set = true;
            Ok(())
        }

        pub fn set_boot_source(
            &mut self,
            _: BootSourceConfig,
        ) -> Result<(), BootSourceConfigError> {
            if self.force_errors {
                return Err(BootSourceConfigError::InvalidKernelPath(
                    std::io::Error::from_raw_os_error(0),
                ));
            }
            self.boot_cfg_set = true;
            Ok(())
        }

        pub fn set_block_device(&mut self, _: BlockDeviceConfig) -> Result<(), DriveError> {
            if self.force_errors {
                return Err(DriveError::RootBlockDeviceAlreadyAdded);
            }
            self.block_set = true;
            Ok(())
        }

        pub fn build_net_device(
            &mut self,
            _: NetworkInterfaceConfig,
        ) -> Result<(), NetworkInterfaceError> {
            if self.force_errors {
                return Err(NetworkInterfaceError::GuestMacAddressInUse(String::new()));
            }
            self.net_set = true;
            Ok(())
        }

        pub fn set_vsock_device(&mut self, _: VsockDeviceConfig) -> Result<(), VsockConfigError> {
            if self.force_errors {
                return Err(VsockConfigError::CreateVsockDevice(
                    VsockError::BufDescMissing,
                ));
            }
            self.vsock_set = true;
            Ok(())
        }

        pub fn set_mmds_config(&mut self, _: MmdsConfig) -> Result<(), MmdsConfigError> {
            if self.force_errors {
                return Err(MmdsConfigError::InvalidIpv4Addr);
            }
            self.mmds_set = true;
            Ok(())
        }
    }

    impl From<&MockVmRes> for VmmConfig {
        fn from(_: &MockVmRes) -> Self {
            VmmConfig::default()
        }
    }

    // Mock `Vmm` used for testing.
    #[derive(Debug, Default, PartialEq)]
    pub struct MockVmm {
        pub balloon_config_called: bool,
        pub latest_balloon_stats_called: bool,
        pub pause_called: bool,
        pub resume_called: bool,
        #[cfg(target_arch = "x86_64")]
        pub send_ctrl_alt_del_called: bool,
        pub update_balloon_config_called: bool,
        pub update_balloon_stats_config_called: bool,
        pub update_block_device_path_called: bool,
        pub update_net_rate_limiters_called: bool,
        // when `true`, all self methods are forced to fail
        pub force_errors: bool,
    }

    impl MockVmm {
        pub fn resume_vm(&mut self) -> Result<(), VmmError> {
            if self.force_errors {
                return Err(VmmError::VcpuResume);
            }
            self.resume_called = true;
            Ok(())
        }

        pub fn pause_vm(&mut self) -> Result<(), VmmError> {
            if self.force_errors {
                return Err(VmmError::VcpuPause);
            }
            self.pause_called = true;
            Ok(())
        }

        #[cfg(target_arch = "x86_64")]
        pub fn send_ctrl_alt_del(&mut self) -> Result<(), VmmError> {
            if self.force_errors {
                return Err(VmmError::I8042Error(
                    devices::legacy::I8042DeviceError::InternalBufferFull,
                ));
            }
            self.send_ctrl_alt_del_called = true;
            Ok(())
        }

        pub fn balloon_config(&mut self) -> Result<BalloonConfig, BalloonError> {
            if self.force_errors {
                return Err(BalloonError::DeviceNotFound);
            }
            self.balloon_config_called = true;
            Ok(BalloonConfig::default())
        }

        pub fn latest_balloon_stats(&mut self) -> Result<BalloonStats, BalloonError> {
            if self.force_errors {
                return Err(BalloonError::DeviceNotFound);
            }
            self.latest_balloon_stats_called = true;
            Ok(BalloonStats::default())
        }

        pub fn update_balloon_config(&mut self, _: u32) -> Result<(), BalloonError> {
            if self.force_errors {
                return Err(BalloonError::DeviceNotFound);
            }
            self.update_balloon_config_called = true;
            Ok(())
        }

        pub fn update_balloon_stats_config(&mut self, _: u16) -> Result<(), BalloonError> {
            if self.force_errors {
                return Err(BalloonError::DeviceNotFound);
            }
            self.update_balloon_stats_config_called = true;
            Ok(())
        }

        pub fn update_block_device_path(&mut self, _: &str, _: String) -> Result<(), VmmError> {
            if self.force_errors {
                return Err(VmmError::DeviceManager(
                    crate::device_manager::mmio::Error::IncorrectDeviceType,
                ));
            }
            self.update_block_device_path_called = true;
            Ok(())
        }

        pub fn update_block_rate_limiter(
            &mut self,
            _: &str,
            _: rate_limiter::BucketUpdate,
            _: rate_limiter::BucketUpdate,
        ) -> Result<(), VmmError> {
            Ok(())
        }

        pub fn update_net_rate_limiters(
            &mut self,
            _: &str,
            _: rate_limiter::BucketUpdate,
            _: rate_limiter::BucketUpdate,
            _: rate_limiter::BucketUpdate,
            _: rate_limiter::BucketUpdate,
        ) -> Result<(), VmmError> {
            if self.force_errors {
                return Err(VmmError::DeviceManager(
                    crate::device_manager::mmio::Error::IncorrectDeviceType,
                ));
            }
            self.update_net_rate_limiters_called = true;
            Ok(())
        }

        pub fn instance_info(&self) -> InstanceInfo {
            InstanceInfo::default()
        }

        pub fn version(&self) -> String {
            String::default()
        }
    }

    // Need to redefine this since the non-test one uses real VmResources
    // and real Vmm instead of our mocks.
    pub fn build_microvm_for_boot(
        _: &InstanceInfo,
        _: &VmResources,
        _: &mut EventManager,
        _: &BpfThreadMap,
    ) -> Result<Arc<Mutex<Vmm>>, StartMicrovmError> {
        Ok(Arc::new(Mutex::new(MockVmm::default())))
    }

    // Need to redefine this since the non-test one uses real Vmm
    // instead of our mocks.
    pub fn create_snapshot(
        _: &mut Vmm,
        _: &CreateSnapshotParams,
        _: versionize::VersionMap,
    ) -> std::result::Result<(), CreateSnapshotError> {
        Ok(())
    }

    // Need to redefine this since the non-test one uses real Vmm
    // instead of our mocks.
    pub fn restore_from_snapshot(
        _: &InstanceInfo,
        _: &mut EventManager,
        _: &BpfThreadMap,
        _: &LoadSnapshotParams,
        _: versionize::VersionMap,
    ) -> Result<Arc<Mutex<Vmm>>, LoadSnapshotError> {
        Ok(Arc::new(Mutex::new(MockVmm::default())))
    }

    fn default_preboot<'a>(
        vm_resources: &'a mut VmResources,
        event_manager: &'a mut EventManager,
        seccomp_filters: &'a BpfThreadMap,
    ) -> PrebootApiController<'a> {
        let instance_info = InstanceInfo::default();
        PrebootApiController::new(seccomp_filters, instance_info, vm_resources, event_manager)
    }

    fn check_preboot_request<F>(request: VmmAction, check_success: F)
    where
        F: FnOnce(ActionResult, &MockVmRes),
    {
        let mut vm_resources = MockVmRes::default();
        let mut evmgr = EventManager::new().unwrap();
        let seccomp_filters = BpfThreadMap::new();
        let mut preboot = default_preboot(&mut vm_resources, &mut evmgr, &seccomp_filters);
        let res = preboot.handle_preboot_request(request);
        check_success(res, &vm_resources);
    }

    // Forces error and validates error kind against expected.
    fn check_preboot_request_err(request: VmmAction, expected_err: VmmActionError) {
        let mut vm_resources = MockVmRes {
            force_errors: true,
            ..Default::default()
        };
        let mut evmgr = EventManager::new().unwrap();
        let seccomp_filters = BpfThreadMap::new();
        let mut preboot = default_preboot(&mut vm_resources, &mut evmgr, &seccomp_filters);
        let err = preboot.handle_preboot_request(request).unwrap_err();
        assert_eq!(err, expected_err);
    }

    #[test]
    fn test_preboot_config_boot_src() {
        let req = VmmAction::ConfigureBootSource(BootSourceConfig::default());
        check_preboot_request(req, |result, vm_res| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert!(vm_res.boot_cfg_set)
        });

        let req = VmmAction::ConfigureBootSource(BootSourceConfig::default());
        check_preboot_request_err(
            req,
            VmmActionError::BootSource(BootSourceConfigError::InvalidKernelCommandLine(
                String::new(),
            )),
        );
    }

    #[test]
    fn test_preboot_get_vm_config() {
        let req = VmmAction::GetVmMachineConfig;
        let expected_cfg = VmConfig::default();
        check_preboot_request(req, |result, _| {
            assert_eq!(result, Ok(VmmData::MachineConfiguration(expected_cfg)))
        });

        let req = VmmAction::ConfigureBootSource(BootSourceConfig::default());
        check_preboot_request_err(
            req,
            VmmActionError::BootSource(BootSourceConfigError::InvalidKernelCommandLine(
                String::new(),
            )),
        );
    }

    #[test]
    fn test_preboot_get_balloon_config() {
        let req = VmmAction::GetBalloonConfig;
        let expected_cfg = BalloonDeviceConfig::default();
        check_preboot_request(req, |result, _| {
            assert_eq!(result, Ok(VmmData::BalloonConfig(expected_cfg)))
        });
    }

    #[test]
    fn test_preboot_set_vm_config() {
        let req = VmmAction::SetVmConfiguration(VmConfig::default());
        let expected_cfg = VmConfig::default();
        check_preboot_request(req, |result, vm_res| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert_eq!(vm_res.vm_config, expected_cfg);
        });

        let req = VmmAction::SetVmConfiguration(VmConfig::default());
        check_preboot_request_err(
            req,
            VmmActionError::MachineConfig(VmConfigError::InvalidVcpuCount),
        );
    }

    #[test]
    fn test_preboot_set_balloon_dev() {
        let req = VmmAction::SetBalloonDevice(BalloonDeviceConfig::default());
        check_preboot_request(req, |result, vm_res| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert!(vm_res.balloon_set)
        });

        let req = VmmAction::SetBalloonDevice(BalloonDeviceConfig::default());
        check_preboot_request_err(
            req,
            VmmActionError::BalloonConfig(BalloonConfigError::DeviceNotFound),
        );
    }

    #[test]
    fn test_preboot_insert_block_dev() {
        let req = VmmAction::InsertBlockDevice(BlockDeviceConfig {
            path_on_host: String::new(),
            is_root_device: false,
            partuuid: None,
            cache_type: CacheType::Unsafe,
            is_read_only: false,
            drive_id: String::new(),
            rate_limiter: None,
        });
        check_preboot_request(req, |result, vm_res| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert!(vm_res.block_set)
        });

        let req = VmmAction::InsertBlockDevice(BlockDeviceConfig {
            path_on_host: String::new(),
            is_root_device: false,
            partuuid: None,
            cache_type: CacheType::Unsafe,
            is_read_only: false,
            drive_id: String::new(),
            rate_limiter: None,
        });
        check_preboot_request_err(
            req,
            VmmActionError::DriveConfig(DriveError::RootBlockDeviceAlreadyAdded),
        );
    }

    #[test]
    fn test_preboot_insert_net_dev() {
        let req = VmmAction::InsertNetworkDevice(NetworkInterfaceConfig {
            iface_id: String::new(),
            host_dev_name: String::new(),
            guest_mac: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
            allow_mmds_requests: false,
        });
        check_preboot_request(req, |result, vm_res| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert!(vm_res.net_set)
        });

        let req = VmmAction::InsertNetworkDevice(NetworkInterfaceConfig {
            iface_id: String::new(),
            host_dev_name: String::new(),
            guest_mac: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
            allow_mmds_requests: false,
        });
        check_preboot_request_err(
            req,
            VmmActionError::NetworkConfig(NetworkInterfaceError::GuestMacAddressInUse(
                String::new(),
            )),
        );
    }

    #[test]
    fn test_preboot_set_vsock_dev() {
        let req = VmmAction::SetVsockDevice(VsockDeviceConfig {
            vsock_id: Some(String::new()),
            guest_cid: 0,
            uds_path: String::new(),
        });
        check_preboot_request(req, |result, vm_res| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert!(vm_res.vsock_set)
        });

        let req = VmmAction::SetVsockDevice(VsockDeviceConfig {
            vsock_id: Some(String::new()),
            guest_cid: 0,
            uds_path: String::new(),
        });
        check_preboot_request_err(
            req,
            VmmActionError::VsockConfig(VsockConfigError::CreateVsockDevice(
                VsockError::BufDescMissing,
            )),
        );
    }

    #[test]
    fn test_preboot_set_mmds_config() {
        let req = VmmAction::SetMmdsConfiguration(MmdsConfig { ipv4_address: None });
        check_preboot_request(req, |result, vm_res| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert!(vm_res.mmds_set)
        });

        let req = VmmAction::SetMmdsConfiguration(MmdsConfig { ipv4_address: None });
        check_preboot_request_err(
            req,
            VmmActionError::MmdsConfig(MmdsConfigError::InvalidIpv4Addr),
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_preboot_load_snapshot() {
        let mut vm_resources = MockVmRes::default();
        let mut evmgr = EventManager::new().unwrap();
        let seccomp_filters = BpfThreadMap::new();
        let mut preboot = default_preboot(&mut vm_resources, &mut evmgr, &seccomp_filters);

        // Without resume.
        let req = VmmAction::LoadSnapshot(LoadSnapshotParams {
            snapshot_path: PathBuf::new(),
            mem_file_path: PathBuf::new(),
            enable_diff_snapshots: false,
            resume_vm: false,
        });
        // Request should succeed.
        preboot.handle_preboot_request(req).unwrap();
        // Should have built default mock vmm.
        let vmm = preboot.built_vmm.take().unwrap();
        assert_eq!(*vmm.lock().unwrap(), MockVmm::default());

        // With resume.
        let req = VmmAction::LoadSnapshot(LoadSnapshotParams {
            snapshot_path: PathBuf::new(),
            mem_file_path: PathBuf::new(),
            enable_diff_snapshots: false,
            resume_vm: true,
        });
        // Request should succeed.
        preboot.handle_preboot_request(req).unwrap();
        let vmm = preboot.built_vmm.as_ref().unwrap().lock().unwrap();
        // Should have built mock vmm then called resume on it.
        assert!(vmm.resume_called);
        // Extra sanity check - pause was never called.
        assert!(!vmm.pause_called);
    }

    #[test]
    fn test_preboot_disallowed() {
        check_preboot_request_err(
            VmmAction::FlushMetrics,
            VmmActionError::OperationNotSupportedPreBoot,
        );
        check_preboot_request_err(
            VmmAction::Pause,
            VmmActionError::OperationNotSupportedPreBoot,
        );
        check_preboot_request_err(
            VmmAction::Resume,
            VmmActionError::OperationNotSupportedPreBoot,
        );
        check_preboot_request_err(
            VmmAction::GetBalloonStats,
            VmmActionError::OperationNotSupportedPreBoot,
        );
        check_preboot_request_err(
            VmmAction::UpdateBalloon(BalloonUpdateConfig { amount_mib: 0 }),
            VmmActionError::OperationNotSupportedPreBoot,
        );
        check_preboot_request_err(
            VmmAction::UpdateBalloonStatistics(BalloonUpdateStatsConfig {
                stats_polling_interval_s: 0,
            }),
            VmmActionError::OperationNotSupportedPreBoot,
        );
        check_preboot_request_err(
            VmmAction::UpdateBlockDevice(BlockDeviceUpdateConfig::default()),
            VmmActionError::OperationNotSupportedPreBoot,
        );
        check_preboot_request_err(
            VmmAction::UpdateNetworkInterface(NetworkInterfaceUpdateConfig {
                iface_id: String::new(),
                rx_rate_limiter: None,
                tx_rate_limiter: None,
            }),
            VmmActionError::OperationNotSupportedPreBoot,
        );
        check_preboot_request_err(
            VmmAction::CreateSnapshot(CreateSnapshotParams {
                snapshot_type: SnapshotType::Full,
                snapshot_path: PathBuf::new(),
                mem_file_path: PathBuf::new(),
                version: None,
            }),
            VmmActionError::OperationNotSupportedPreBoot,
        );
        #[cfg(target_arch = "x86_64")]
        check_preboot_request_err(
            VmmAction::SendCtrlAltDel,
            VmmActionError::OperationNotSupportedPreBoot,
        );
    }

    #[test]
    fn test_build_microvm_from_requests() {
        // Use atomics to be able to use them non-mutably in closures below.
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cmd_step = AtomicUsize::new(0);
        let commands = || {
            cmd_step.fetch_add(1, Ordering::SeqCst);
            match cmd_step.load(Ordering::SeqCst) {
                1 => VmmAction::FlushMetrics,
                2 => VmmAction::Pause,
                3 => VmmAction::Resume,
                4 => VmmAction::StartMicroVm,
                _ => unreachable!(),
            }
        };

        let resp_step = AtomicUsize::new(0);
        let expected_resp = |resp: ActionResult| {
            resp_step.fetch_add(1, Ordering::SeqCst);
            let expect = match resp_step.load(Ordering::SeqCst) {
                1 => Err(VmmActionError::OperationNotSupportedPreBoot),
                2 => Err(VmmActionError::OperationNotSupportedPreBoot),
                3 => Err(VmmActionError::OperationNotSupportedPreBoot),
                4 => Ok(VmmData::Empty),
                _ => unreachable!(),
            };
            assert_eq!(resp, expect);
        };

        let (_vm_res, _vmm) = PrebootApiController::build_microvm_from_requests(
            &BpfThreadMap::new(),
            &mut EventManager::new().unwrap(),
            InstanceInfo::default(),
            commands,
            expected_resp,
            false,
        )
        .unwrap();
    }

    fn check_runtime_request<F>(request: VmmAction, check_success: F)
    where
        F: FnOnce(ActionResult, &MockVmm),
    {
        let vmm = Arc::new(Mutex::new(MockVmm::default()));
        let mut runtime = RuntimeApiController::new(MockVmRes::default(), vmm.clone());
        let res = runtime.handle_request(request);
        check_success(res, &vmm.lock().unwrap());
    }

    // Forces error and validates error kind against expected.
    fn check_runtime_request_err(request: VmmAction, expected_err: VmmActionError) {
        let vmm = Arc::new(Mutex::new(MockVmm {
            force_errors: true,
            ..Default::default()
        }));
        let mut runtime = RuntimeApiController::new(MockVmRes::default(), vmm);
        let err = runtime.handle_request(request).unwrap_err();
        assert_eq!(err, expected_err);
    }

    #[test]
    fn test_runtime_get_vm_config() {
        let req = VmmAction::GetVmMachineConfig;
        check_runtime_request(req, |result, _| {
            assert_eq!(
                result,
                Ok(VmmData::MachineConfiguration(VmConfig::default()))
            );
        });
    }

    #[test]
    fn test_runtime_pause() {
        let req = VmmAction::Pause;
        check_runtime_request(req, |result, vmm| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert!(vmm.pause_called)
        });

        let req = VmmAction::Pause;
        check_runtime_request_err(req, VmmActionError::InternalVmm(VmmError::VcpuPause));
    }

    #[test]
    fn test_runtime_resume() {
        let req = VmmAction::Resume;
        check_runtime_request(req, |result, vmm| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert!(vmm.resume_called)
        });

        let req = VmmAction::Resume;
        check_runtime_request_err(req, VmmActionError::InternalVmm(VmmError::VcpuResume));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn test_runtime_ctrl_alt_del() {
        let req = VmmAction::SendCtrlAltDel;
        check_runtime_request(req, |result, vmm| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert!(vmm.send_ctrl_alt_del_called)
        });

        let req = VmmAction::SendCtrlAltDel;
        check_runtime_request_err(
            req,
            VmmActionError::InternalVmm(VmmError::I8042Error(
                devices::legacy::I8042DeviceError::InternalBufferFull,
            )),
        );
    }

    #[test]
    fn test_runtime_balloon_config() {
        let req = VmmAction::GetBalloonConfig;
        check_runtime_request(req, |result, vmm| {
            assert_eq!(
                result,
                Ok(VmmData::BalloonConfig(BalloonDeviceConfig::default()))
            );
            assert!(vmm.balloon_config_called)
        });

        let req = VmmAction::GetBalloonConfig;
        check_runtime_request_err(
            req,
            VmmActionError::BalloonConfig(BalloonConfigError::DeviceNotFound),
        );
    }

    #[test]
    fn test_runtime_latest_balloon_stats() {
        let req = VmmAction::GetBalloonStats;
        check_runtime_request(req, |result, vmm| {
            assert_eq!(result, Ok(VmmData::BalloonStats(BalloonStats::default())));
            assert!(vmm.latest_balloon_stats_called)
        });

        let req = VmmAction::GetBalloonStats;
        check_runtime_request_err(
            req,
            VmmActionError::BalloonConfig(BalloonConfigError::DeviceNotFound),
        );
    }

    #[test]
    fn test_runtime_update_balloon_config() {
        let req = VmmAction::UpdateBalloon(BalloonUpdateConfig { amount_mib: 0 });
        check_runtime_request(req, |result, vmm| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert!(vmm.update_balloon_config_called)
        });

        let req = VmmAction::UpdateBalloon(BalloonUpdateConfig { amount_mib: 0 });
        check_runtime_request_err(
            req,
            VmmActionError::BalloonConfig(BalloonConfigError::DeviceNotFound),
        );
    }

    #[test]
    fn test_runtime_update_balloon_stats_config() {
        let req = VmmAction::UpdateBalloonStatistics(BalloonUpdateStatsConfig {
            stats_polling_interval_s: 0,
        });
        check_runtime_request(req, |result, vmm| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert!(vmm.update_balloon_stats_config_called)
        });

        let req = VmmAction::UpdateBalloonStatistics(BalloonUpdateStatsConfig {
            stats_polling_interval_s: 0,
        });
        check_runtime_request_err(
            req,
            VmmActionError::BalloonConfig(BalloonConfigError::DeviceNotFound),
        );
    }

    #[test]
    fn test_runtime_update_block_device_path() {
        let req = VmmAction::UpdateBlockDevice(BlockDeviceUpdateConfig {
            path_on_host: Some(String::new()),
            ..Default::default()
        });
        check_runtime_request(req, |result, vmm| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert!(vmm.update_block_device_path_called)
        });

        let req = VmmAction::UpdateBlockDevice(BlockDeviceUpdateConfig {
            path_on_host: Some(String::new()),
            ..Default::default()
        });
        check_runtime_request_err(
            req,
            VmmActionError::DriveConfig(DriveError::DeviceUpdate(VmmError::DeviceManager(
                crate::device_manager::mmio::Error::IncorrectDeviceType,
            ))),
        );
    }

    #[test]
    fn test_runtime_update_net_rate_limiters() {
        let req = VmmAction::UpdateNetworkInterface(NetworkInterfaceUpdateConfig {
            iface_id: String::new(),
            rx_rate_limiter: None,
            tx_rate_limiter: None,
        });
        check_runtime_request(req, |result, vmm| {
            assert_eq!(result, Ok(VmmData::Empty));
            assert!(vmm.update_net_rate_limiters_called)
        });

        let req = VmmAction::UpdateNetworkInterface(NetworkInterfaceUpdateConfig {
            iface_id: String::new(),
            rx_rate_limiter: None,
            tx_rate_limiter: None,
        });
        check_runtime_request_err(
            req,
            VmmActionError::NetworkConfig(NetworkInterfaceError::DeviceUpdate(
                VmmError::DeviceManager(crate::device_manager::mmio::Error::IncorrectDeviceType),
            )),
        );
    }

    #[test]
    fn test_runtime_disallowed() {
        check_runtime_request_err(
            VmmAction::ConfigureBootSource(BootSourceConfig::default()),
            VmmActionError::OperationNotSupportedPostBoot,
        );
        check_runtime_request_err(
            VmmAction::ConfigureLogger(LoggerConfig {
                log_path: PathBuf::new(),
                level: LoggerLevel::Debug,
                show_level: false,
                show_log_origin: false,
            }),
            VmmActionError::OperationNotSupportedPostBoot,
        );
        check_runtime_request_err(
            VmmAction::ConfigureMetrics(MetricsConfig {
                metrics_path: PathBuf::new(),
            }),
            VmmActionError::OperationNotSupportedPostBoot,
        );
        check_runtime_request_err(
            VmmAction::InsertBlockDevice(BlockDeviceConfig {
                path_on_host: String::new(),
                is_root_device: false,
                partuuid: None,
                cache_type: CacheType::Unsafe,
                is_read_only: false,
                drive_id: String::new(),
                rate_limiter: None,
            }),
            VmmActionError::OperationNotSupportedPostBoot,
        );
        check_runtime_request_err(
            VmmAction::InsertNetworkDevice(NetworkInterfaceConfig {
                iface_id: String::new(),
                host_dev_name: String::new(),
                guest_mac: None,
                rx_rate_limiter: None,
                tx_rate_limiter: None,
                allow_mmds_requests: false,
            }),
            VmmActionError::OperationNotSupportedPostBoot,
        );
        check_runtime_request_err(
            VmmAction::SetVsockDevice(VsockDeviceConfig {
                vsock_id: Some(String::new()),
                guest_cid: 0,
                uds_path: String::new(),
            }),
            VmmActionError::OperationNotSupportedPostBoot,
        );
        check_runtime_request_err(
            VmmAction::SetBalloonDevice(BalloonDeviceConfig::default()),
            VmmActionError::OperationNotSupportedPostBoot,
        );
        check_runtime_request_err(
            VmmAction::SetVsockDevice(VsockDeviceConfig {
                vsock_id: Some(String::new()),
                guest_cid: 0,
                uds_path: String::new(),
            }),
            VmmActionError::OperationNotSupportedPostBoot,
        );
        check_runtime_request_err(
            VmmAction::SetMmdsConfiguration(MmdsConfig { ipv4_address: None }),
            VmmActionError::OperationNotSupportedPostBoot,
        );
        check_runtime_request_err(
            VmmAction::SetVmConfiguration(VmConfig::default()),
            VmmActionError::OperationNotSupportedPostBoot,
        );
        check_runtime_request_err(
            VmmAction::LoadSnapshot(LoadSnapshotParams {
                snapshot_path: PathBuf::new(),
                mem_file_path: PathBuf::new(),
                enable_diff_snapshots: false,
                resume_vm: false,
            }),
            VmmActionError::OperationNotSupportedPostBoot,
        );
    }

    fn verify_load_snap_disallowed_after_boot_resources(res: VmmAction, res_name: &str) {
        let mut vm_resources = MockVmRes::default();
        let mut evmgr = EventManager::new().unwrap();
        let seccomp_filters = BpfThreadMap::new();
        let mut preboot = default_preboot(&mut vm_resources, &mut evmgr, &seccomp_filters);

        preboot.handle_preboot_request(res).unwrap();

        // Load snapshot should no longer be allowed.
        let req = VmmAction::LoadSnapshot(LoadSnapshotParams {
            snapshot_path: PathBuf::new(),
            mem_file_path: PathBuf::new(),
            enable_diff_snapshots: false,
            resume_vm: false,
        });
        let err = preboot.handle_preboot_request(req);
        assert_eq!(
            err,
            Err(VmmActionError::LoadSnapshotNotAllowed),
            "LoadSnapshot should be disallowed after {}",
            res_name
        );
    }

    #[test]
    fn test_preboot_load_snap_disallowed_after_boot_resources() {
        // Verify LoadSnapshot not allowed after configuring various boot-specific resources.
        let req = VmmAction::ConfigureBootSource(BootSourceConfig::default());
        verify_load_snap_disallowed_after_boot_resources(req, "ConfigureBootSource");

        let req = VmmAction::InsertBlockDevice(BlockDeviceConfig {
            path_on_host: String::new(),
            is_root_device: false,
            partuuid: None,
            cache_type: CacheType::Unsafe,
            is_read_only: false,
            drive_id: String::new(),
            rate_limiter: None,
        });
        verify_load_snap_disallowed_after_boot_resources(req, "InsertBlockDevice");

        let req = VmmAction::InsertNetworkDevice(NetworkInterfaceConfig {
            iface_id: String::new(),
            host_dev_name: String::new(),
            guest_mac: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
            allow_mmds_requests: false,
        });
        verify_load_snap_disallowed_after_boot_resources(req, "InsertNetworkDevice");

        let req = VmmAction::SetBalloonDevice(BalloonDeviceConfig::default());
        verify_load_snap_disallowed_after_boot_resources(req, "SetBalloonDevice");

        let req = VmmAction::SetVsockDevice(VsockDeviceConfig {
            vsock_id: Some(String::new()),
            guest_cid: 0,
            uds_path: String::new(),
        });
        verify_load_snap_disallowed_after_boot_resources(req, "SetVsockDevice");

        let req = VmmAction::SetVmConfiguration(VmConfig::default());
        verify_load_snap_disallowed_after_boot_resources(req, "SetVmConfiguration");

        let req = VmmAction::SetMmdsConfiguration(MmdsConfig { ipv4_address: None });
        verify_load_snap_disallowed_after_boot_resources(req, "SetMmdsConfiguration");
    }
}
