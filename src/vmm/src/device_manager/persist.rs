// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Provides functionality for saving/restoring the MMIO device manager and its devices.

use std::result::Result;
use std::sync::{Arc, Mutex};

use super::mmio::*;
use crate::EventManager;
use logger::{error, warn};

use crate::resources::VmResources;
use crate::vmm_config::mmds::MmdsConfigError;
#[cfg(target_arch = "aarch64")]
use arch::DeviceType;
use devices::virtio::balloon::persist::{BalloonConstructorArgs, BalloonState};
use devices::virtio::balloon::{Balloon, Error as BalloonError};
use devices::virtio::block::persist::{BlockConstructorArgs, BlockState};
use devices::virtio::block::{Block, Error as BlockError};
use devices::virtio::net::persist::{Error as NetError, NetConstructorArgs, NetState};
use devices::virtio::net::Net;
use devices::virtio::persist::{MmioTransportConstructorArgs, MmioTransportState};
use devices::virtio::vsock::persist::{VsockConstructorArgs, VsockState, VsockUdsConstructorArgs};
use devices::virtio::vsock::{Vsock, VsockError, VsockUnixBackend, VsockUnixBackendError};
use devices::virtio::{
    MmioTransport, VirtioDevice, TYPE_BALLOON, TYPE_BLOCK, TYPE_NET, TYPE_VSOCK,
};
use event_manager::{MutEventSubscriber, SubscriberOps};
use kvm_ioctls::VmFd;
use mmds::data_store::MmdsVersion;
use snapshot::Persist;
use versionize::{VersionMap, Versionize, VersionizeError, VersionizeResult};
use versionize_derive::Versionize;
use vm_memory::GuestMemoryMmap;

/// Errors for (de)serialization of the MMIO device manager.
#[derive(Debug)]
pub enum Error {
    Balloon(BalloonError),
    Block(BlockError),
    DeviceManager(super::mmio::Error),
    MmioTransport,
    #[cfg(target_arch = "aarch64")]
    Legacy(crate::Error),
    Net(NetError),
    Vsock(VsockError),
    VsockUnixBackend(VsockUnixBackendError),
    MmdsConfig(MmdsConfigError),
}

#[derive(Clone, Versionize)]
/// Holds the state of a balloon device connected to the MMIO space.
// NOTICE: Any changes to this structure require a snapshot version bump.
pub struct ConnectedBalloonState {
    /// Device identifier.
    pub device_id: String,
    /// Device state.
    pub device_state: BalloonState,
    /// Mmio transport state.
    pub transport_state: MmioTransportState,
    /// VmmResources.
    pub mmio_slot: MMIODeviceInfo,
}

#[derive(Clone, Versionize)]
/// Holds the state of a block device connected to the MMIO space.
// NOTICE: Any changes to this structure require a snapshot version bump.
pub struct ConnectedBlockState {
    /// Device identifier.
    pub device_id: String,
    /// Device state.
    pub device_state: BlockState,
    /// Mmio transport state.
    pub transport_state: MmioTransportState,
    /// VmmResources.
    pub mmio_slot: MMIODeviceInfo,
}

#[derive(Clone, Versionize)]
/// Holds the state of a net device connected to the MMIO space.
// NOTICE: Any changes to this structure require a snapshot version bump.
pub struct ConnectedNetState {
    /// Device identifier.
    pub device_id: String,
    /// Device state.
    pub device_state: NetState,
    /// Mmio transport state.
    pub transport_state: MmioTransportState,
    /// VmmResources.
    pub mmio_slot: MMIODeviceInfo,
}

#[derive(Clone, Versionize)]
/// Holds the state of a vsock device connected to the MMIO space.
// NOTICE: Any changes to this structure require a snapshot version bump.
pub struct ConnectedVsockState {
    /// Device identifier.
    pub device_id: String,
    /// Device state.
    pub device_state: VsockState,
    /// Mmio transport state.
    pub transport_state: MmioTransportState,
    /// VmmResources.
    pub mmio_slot: MMIODeviceInfo,
}

#[cfg(target_arch = "aarch64")]
#[derive(Clone, Versionize)]
/// Holds the state of a legacy device connected to the MMIO space.
pub struct ConnectedLegacyState {
    /// Device identifier.
    pub type_: DeviceType,
    /// VmmResources.
    pub mmio_slot: MMIODeviceInfo,
}

/// Holds the MMDS data store version.
#[derive(Debug, PartialEq, Versionize, Clone)]
// NOTICE: Any changes to this structure require a snapshot version bump.
pub enum MmdsVersionState {
    V1,
    V2,
}

impl From<MmdsVersionState> for MmdsVersion {
    fn from(state: MmdsVersionState) -> Self {
        match state {
            MmdsVersionState::V1 => MmdsVersion::V1,
            MmdsVersionState::V2 => MmdsVersion::V2,
        }
    }
}

impl From<MmdsVersion> for MmdsVersionState {
    fn from(version: MmdsVersion) -> Self {
        match version {
            MmdsVersion::V1 => MmdsVersionState::V1,
            MmdsVersion::V2 => MmdsVersionState::V2,
        }
    }
}

#[derive(Clone, Versionize)]
/// Holds the device states.
// NOTICE: Any changes to this structure require a snapshot version bump.
pub struct DeviceStates {
    #[cfg(target_arch = "aarch64")]
    // State of legacy devices in MMIO space.
    pub legacy_devices: Vec<ConnectedLegacyState>,
    /// Block device states.
    pub block_devices: Vec<ConnectedBlockState>,
    /// Net device states.
    pub net_devices: Vec<ConnectedNetState>,
    /// Vsock device state.
    pub vsock_device: Option<ConnectedVsockState>,
    /// Balloon device state.
    #[version(start = 2, ser_fn = "balloon_serialize")]
    pub balloon_device: Option<ConnectedBalloonState>,
    /// Mmds version.
    #[version(start = 3, ser_fn = "mmds_version_serialize")]
    pub mmds_version: Option<MmdsVersionState>,
}

/// A type used to extract the concrete Arc<Mutex<T>> for each of the device types when restoring
/// from a snapshot.
pub enum SharedDeviceType {
    SharedBlock(Arc<Mutex<Block>>),
    SharedNetwork(Arc<Mutex<Net>>),
    SharedBalloon(Arc<Mutex<Balloon>>),
    SharedVsock(Arc<Mutex<Vsock<VsockUnixBackend>>>),
}

impl DeviceStates {
    fn balloon_serialize(&mut self, target_version: u16) -> VersionizeResult<()> {
        if target_version < 2 && self.balloon_device.is_some() {
            return Err(VersionizeError::Semantic(
                "Target version does not implement the virtio-balloon device.".to_owned(),
            ));
        }

        Ok(())
    }

    fn mmds_version_serialize(&mut self, target_version: u16) -> VersionizeResult<()> {
        if target_version < 3 && self.mmds_version.is_some() {
            warn!(
                "Target version does not support persisting the MMDS version. The default will be \
                used when restoring."
            );
        }

        Ok(())
    }
}

pub struct MMIODevManagerConstructorArgs<'a> {
    pub mem: GuestMemoryMmap,
    pub vm: &'a VmFd,
    pub event_manager: &'a mut EventManager,
    pub for_each_restored_device: fn(&mut VmResources, SharedDeviceType),
    pub vm_resources: &'a mut VmResources,
    pub instance_id: &'a str,
}

impl<'a> Persist<'a> for MMIODeviceManager {
    type State = DeviceStates;
    type ConstructorArgs = MMIODevManagerConstructorArgs<'a>;
    type Error = Error;

    fn save(&self) -> Self::State {
        let mut states = DeviceStates {
            balloon_device: None,
            block_devices: Vec::new(),
            net_devices: Vec::new(),
            vsock_device: None,
            #[cfg(target_arch = "aarch64")]
            legacy_devices: Vec::new(),
            mmds_version: None,
        };
        let _: Result<(), ()> = self.for_each_device(|devtype, devid, devinfo, bus_dev| {
            if *devtype == arch::DeviceType::BootTimer {
                // No need to save BootTimer state.
                return Ok(());
            }

            #[cfg(target_arch = "aarch64")]
            {
                if *devtype == DeviceType::Serial || *devtype == DeviceType::Rtc {
                    states.legacy_devices.push(ConnectedLegacyState {
                        type_: *devtype,
                        mmio_slot: devinfo.clone(),
                    });
                    return Ok(());
                }
            }

            let locked_bus_dev = bus_dev.lock().expect("Poisoned lock");
            let mmio_transport = locked_bus_dev
                .as_any()
                // Only MmioTransport implements BusDevice on x86_64 at this point.
                .downcast_ref::<MmioTransport>()
                .expect("Unexpected BusDevice type");

            let transport_state = mmio_transport.save();

            let mut locked_device = mmio_transport.locked_device();
            match locked_device.device_type() {
                TYPE_BALLOON => {
                    let balloon_state = locked_device
                        .as_any()
                        .downcast_ref::<Balloon>()
                        .unwrap()
                        .save();
                    states.balloon_device = Some(ConnectedBalloonState {
                        device_id: devid.clone(),
                        device_state: balloon_state,
                        transport_state,
                        mmio_slot: devinfo.clone(),
                    });
                }
                TYPE_BLOCK => {
                    let block = locked_device.as_mut_any().downcast_mut::<Block>().unwrap();
                    block.prepare_save();
                    states.block_devices.push(ConnectedBlockState {
                        device_id: devid.clone(),
                        device_state: block.save(),
                        transport_state,
                        mmio_slot: devinfo.clone(),
                    });
                }
                TYPE_NET => {
                    let net = locked_device.as_any().downcast_ref::<Net>().unwrap();
                    if let (Some(mmds_ns), None) =
                        (net.mmds_ns.as_ref(), states.mmds_version.as_ref())
                    {
                        states.mmds_version =
                            Some(mmds_ns.mmds.lock().expect("Poisoned lock").version().into());
                    }

                    states.net_devices.push(ConnectedNetState {
                        device_id: devid.clone(),
                        device_state: net.save(),
                        transport_state,
                        mmio_slot: devinfo.clone(),
                    });
                }
                TYPE_VSOCK => {
                    let vsock = locked_device
                        .as_mut_any()
                        // Currently, VsockUnixBackend is the only implementation of VsockBackend.
                        .downcast_mut::<Vsock<VsockUnixBackend>>()
                        .unwrap();

                    let vsock_state = VsockState {
                        backend: vsock.backend().save(),
                        frontend: vsock.save(),
                    };

                    // Send Transport event to reset connections if device
                    // is activated.
                    if vsock.is_activated() {
                        vsock.send_transport_reset_event().unwrap_or_else(|e| {
                            error!("Failed to send reset transport event: {:?}", e);
                        });
                    }

                    states.vsock_device = Some(ConnectedVsockState {
                        device_id: devid.clone(),
                        device_state: vsock_state,
                        transport_state,
                        mmio_slot: devinfo.clone(),
                    });
                }
                _ => unreachable!(),
            };

            Ok(())
        });
        states
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> Result<Self, Self::Error> {
        let mut dev_manager =
            MMIODeviceManager::new(arch::MMIO_MEM_START, (arch::IRQ_BASE, arch::IRQ_MAX));
        let mem = &constructor_args.mem;
        let vm = constructor_args.vm;

        #[cfg(target_arch = "aarch64")]
        {
            for state in &state.legacy_devices {
                if state.type_ == DeviceType::Serial {
                    let serial = crate::builder::setup_serial_device(
                        constructor_args.event_manager,
                        Box::new(crate::builder::SerialStdin::get()),
                        Box::new(std::io::stdout()),
                    )
                    .map_err(Error::Legacy)?;

                    dev_manager
                        .register_mmio_serial(vm, serial, Some(state.mmio_slot.clone()))
                        .map_err(Error::DeviceManager)?;
                }
                if state.type_ == DeviceType::Rtc {
                    let rtc = crate::builder::setup_rtc_device();
                    dev_manager
                        .register_mmio_rtc(rtc, Some(state.mmio_slot.clone()))
                        .map_err(Error::DeviceManager)?;
                }
            }
        }

        let mut restore_helper = |device: Arc<Mutex<dyn VirtioDevice>>,
                                  as_subscriber: Arc<Mutex<dyn MutEventSubscriber>>,
                                  id: &String,
                                  state: &MmioTransportState,
                                  slot: &MMIODeviceInfo,
                                  event_manager: &mut EventManager|
         -> Result<(), Self::Error> {
            dev_manager
                .slot_sanity_check(slot)
                .map_err(Error::DeviceManager)?;

            let restore_args = MmioTransportConstructorArgs {
                mem: mem.clone(),
                device,
            };
            let mmio_transport =
                MmioTransport::restore(restore_args, state).map_err(|()| Error::MmioTransport)?;
            dev_manager
                .register_mmio_virtio(vm, id.clone(), mmio_transport, slot)
                .map_err(Error::DeviceManager)?;

            event_manager.add_subscriber(as_subscriber);
            Ok(())
        };

        if let Some(balloon_state) = &state.balloon_device {
            let device = Arc::new(Mutex::new(
                Balloon::restore(
                    BalloonConstructorArgs { mem: mem.clone() },
                    &balloon_state.device_state,
                )
                .map_err(Error::Balloon)?,
            ));

            (constructor_args.for_each_restored_device)(
                constructor_args.vm_resources,
                SharedDeviceType::SharedBalloon(device.clone()),
            );

            restore_helper(
                device.clone(),
                device,
                &balloon_state.device_id,
                &balloon_state.transport_state,
                &balloon_state.mmio_slot,
                constructor_args.event_manager,
            )?;
        }

        for block_state in &state.block_devices {
            let device = Arc::new(Mutex::new(
                Block::restore(
                    BlockConstructorArgs { mem: mem.clone() },
                    &block_state.device_state,
                )
                .map_err(Error::Block)?,
            ));

            (constructor_args.for_each_restored_device)(
                constructor_args.vm_resources,
                SharedDeviceType::SharedBlock(device.clone()),
            );

            restore_helper(
                device.clone(),
                device,
                &block_state.device_id,
                &block_state.transport_state,
                &block_state.mmio_slot,
                constructor_args.event_manager,
            )?;
        }

        // If the snapshot has the mmds version persisted, initialise the data store with it.
        if let Some(mmds_version) = &state.mmds_version {
            constructor_args
                .vm_resources
                .set_mmds_version(mmds_version.clone().into(), constructor_args.instance_id)
                .map_err(Error::MmdsConfig)?;
        } else if state
            .net_devices
            .iter()
            .any(|dev| dev.device_state.mmds_ns.is_some())
        {
            // If there's at least one network device having an mmds_ns, it means
            // that we are restoring from a version that did not persist the `MmdsVersionState`.
            // Init with the default.
            constructor_args.vm_resources.mmds_or_default();
        }

        for net_state in &state.net_devices {
            let device = Arc::new(Mutex::new(
                Net::restore(
                    NetConstructorArgs {
                        mem: mem.clone(),
                        mmds: constructor_args
                            .vm_resources
                            .mmds
                            .as_ref()
                            // Clone the Arc reference.
                            .cloned(),
                    },
                    &net_state.device_state,
                )
                .map_err(Error::Net)?,
            ));

            (constructor_args.for_each_restored_device)(
                constructor_args.vm_resources,
                SharedDeviceType::SharedNetwork(device.clone()),
            );

            restore_helper(
                device.clone(),
                device,
                &net_state.device_id,
                &net_state.transport_state,
                &net_state.mmio_slot,
                constructor_args.event_manager,
            )?;
        }

        if let Some(vsock_state) = &state.vsock_device {
            let ctor_args = VsockUdsConstructorArgs {
                cid: vsock_state.device_state.frontend.cid,
            };
            let backend = VsockUnixBackend::restore(ctor_args, &vsock_state.device_state.backend)
                .map_err(Error::VsockUnixBackend)?;
            let device = Arc::new(Mutex::new(
                Vsock::restore(
                    VsockConstructorArgs {
                        mem: mem.clone(),
                        backend,
                    },
                    &vsock_state.device_state.frontend,
                )
                .map_err(Error::Vsock)?,
            ));

            (constructor_args.for_each_restored_device)(
                constructor_args.vm_resources,
                SharedDeviceType::SharedVsock(device.clone()),
            );

            restore_helper(
                device.clone(),
                device,
                &vsock_state.device_id,
                &vsock_state.transport_state,
                &vsock_state.mmio_slot,
                constructor_args.event_manager,
            )?;
        }

        Ok(dev_manager)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::tests::*;
    use crate::resources::VmmConfig;
    use crate::vmm_config::balloon::BalloonDeviceConfig;
    use crate::vmm_config::net::NetworkInterfaceConfig;
    use crate::vmm_config::vsock::VsockDeviceConfig;
    use devices::virtio::block::CacheType;
    use utils::tempfile::TempFile;

    impl PartialEq for ConnectedBalloonState {
        fn eq(&self, other: &ConnectedBalloonState) -> bool {
            // Actual device state equality is checked by the device's tests.
            self.transport_state == other.transport_state && self.mmio_slot == other.mmio_slot
        }
    }

    impl std::fmt::Debug for ConnectedBalloonState {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(
                f,
                "ConnectedBalloonDevice {{ transport_state: {:?}, mmio_slot: {:?} }}",
                self.transport_state, self.mmio_slot
            )
        }
    }

    impl PartialEq for ConnectedBlockState {
        fn eq(&self, other: &ConnectedBlockState) -> bool {
            // Actual device state equality is checked by the device's tests.
            self.transport_state == other.transport_state && self.mmio_slot == other.mmio_slot
        }
    }

    impl std::fmt::Debug for ConnectedBlockState {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(
                f,
                "ConnectedBlockDevice {{ transport_state: {:?}, mmio_slot: {:?} }}",
                self.transport_state, self.mmio_slot
            )
        }
    }

    impl PartialEq for ConnectedNetState {
        fn eq(&self, other: &ConnectedNetState) -> bool {
            // Actual device state equality is checked by the device's tests.
            self.transport_state == other.transport_state && self.mmio_slot == other.mmio_slot
        }
    }

    impl std::fmt::Debug for ConnectedNetState {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(
                f,
                "ConnectedNetDevice {{ transport_state: {:?}, mmio_slot: {:?} }}",
                self.transport_state, self.mmio_slot
            )
        }
    }

    impl PartialEq for ConnectedVsockState {
        fn eq(&self, other: &ConnectedVsockState) -> bool {
            // Actual device state equality is checked by the device's tests.
            self.transport_state == other.transport_state && self.mmio_slot == other.mmio_slot
        }
    }

    impl std::fmt::Debug for ConnectedVsockState {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(
                f,
                "ConnectedVsockDevice {{ transport_state: {:?}, mmio_slot: {:?} }}",
                self.transport_state, self.mmio_slot
            )
        }
    }

    impl PartialEq for DeviceStates {
        fn eq(&self, other: &DeviceStates) -> bool {
            self.balloon_device == other.balloon_device
                && self.block_devices == other.block_devices
                && self.net_devices == other.net_devices
                && self.vsock_device == other.vsock_device
        }
    }

    impl std::fmt::Debug for DeviceStates {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(
                f,
                "DevicesStates {{ block_devices: {:?}, net_devices: {:?}, vsock_device: {:?} }}",
                self.block_devices, self.net_devices, self.vsock_device
            )
        }
    }

    impl MMIODeviceManager {
        fn soft_clone(&self) -> Self {
            let dummy_mmio_base = 0;
            let dummy_irq_range = (0, 0);
            let mut clone = MMIODeviceManager::new(dummy_mmio_base, dummy_irq_range);
            // We only care about the device hashmap.
            clone.id_to_dev_info = self.id_to_dev_info.clone();
            clone
        }
    }

    impl PartialEq for MMIODeviceManager {
        fn eq(&self, other: &MMIODeviceManager) -> bool {
            // We only care about the device hashmap.
            if self.id_to_dev_info.len() != other.id_to_dev_info.len() {
                return false;
            }
            for (key, val) in &self.id_to_dev_info {
                match other.id_to_dev_info.get(key) {
                    Some(other_val) if val == other_val => continue,
                    _ => return false,
                };
            }
            true
        }
    }

    impl std::fmt::Debug for MMIODeviceManager {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "{:?}", self.id_to_dev_info)
        }
    }

    #[test]
    fn test_device_manager_persistence() {
        let mut buf = vec![0; 16384];
        let mut version_map = VersionMap::new();
        // These need to survive so the restored blocks find them.
        let _block_files;
        let mut tmp_sock_file = TempFile::new().unwrap();
        tmp_sock_file.remove().unwrap();
        // Set up a vmm with one of each device, and get the serialized DeviceStates.
        let original_mmio_device_manager = {
            let mut event_manager = EventManager::new().expect("Unable to create EventManager");
            let mut vmm = default_vmm();
            let mut cmdline = default_kernel_cmdline();

            // Add a balloon device.
            let balloon_cfg = BalloonDeviceConfig {
                amount_mib: 123,
                deflate_on_oom: false,
                stats_polling_interval_s: 1,
            };
            insert_balloon_device(&mut vmm, &mut cmdline, &mut event_manager, balloon_cfg);
            // Add a block device.
            let drive_id = String::from("root");
            let block_configs = vec![CustomBlockConfig::new(
                drive_id,
                true,
                None,
                true,
                CacheType::Unsafe,
            )];
            _block_files =
                insert_block_devices(&mut vmm, &mut cmdline, &mut event_manager, block_configs);
            // Add a net device.
            let network_interface = NetworkInterfaceConfig {
                iface_id: String::from("netif"),
                host_dev_name: String::from("hostname"),
                guest_mac: None,
                rx_rate_limiter: None,
                tx_rate_limiter: None,
            };
            insert_net_device_with_mmds(
                &mut vmm,
                &mut cmdline,
                &mut event_manager,
                network_interface,
                MmdsVersion::V2,
            );
            // Add a vsock device.
            let vsock_dev_id = "vsock";
            let vsock_config = VsockDeviceConfig {
                vsock_id: Some(vsock_dev_id.to_string()),
                guest_cid: 3,
                uds_path: tmp_sock_file.as_path().to_str().unwrap().to_string(),
            };
            insert_vsock_device(&mut vmm, &mut cmdline, &mut event_manager, vsock_config);

            assert_eq!(
                vmm.mmio_device_manager
                    .save()
                    .serialize(&mut buf.as_mut_slice(), &version_map, 1),
                Err(VersionizeError::Semantic(
                    "Target version does not implement the virtio-balloon device.".to_string()
                ))
            );

            version_map
                .new_version()
                .set_type_version(DeviceStates::type_id(), 2);
            vmm.mmio_device_manager
                .save()
                .serialize(&mut buf.as_mut_slice(), &version_map, 2)
                .unwrap();

            version_map
                .new_version()
                .set_type_version(DeviceStates::type_id(), 3);

            // For snapshot versions that not support persisting the mmds version, it should be
            // deserialized as None. The MMIODeviceManager will initialise it as the default if
            // there's at least one network device having a MMDS NS.
            vmm.mmio_device_manager
                .save()
                .serialize(&mut buf.as_mut_slice(), &version_map, 2)
                .unwrap();
            let device_states: DeviceStates =
                DeviceStates::deserialize(&mut buf.as_slice(), &version_map, 2).unwrap();
            assert!(device_states.mmds_version.is_none());

            vmm.mmio_device_manager
                .save()
                .serialize(&mut buf.as_mut_slice(), &version_map, 3)
                .unwrap();

            // We only want to keep the device map from the original MmioDeviceManager.
            vmm.mmio_device_manager.soft_clone()
        };
        tmp_sock_file.remove().unwrap();

        let mut event_manager = EventManager::new().expect("Unable to create EventManager");
        let vmm = default_vmm();
        let device_states: DeviceStates =
            DeviceStates::deserialize(&mut buf.as_slice(), &version_map, 3).unwrap();
        let vm_resources = &mut VmResources::default();
        let restore_args = MMIODevManagerConstructorArgs {
            mem: vmm.guest_memory().clone(),
            vm: vmm.vm.fd(),
            event_manager: &mut event_manager,
            for_each_restored_device: VmResources::update_from_restored_device,
            vm_resources,
            instance_id: "microvm-id",
        };
        let restored_dev_manager =
            MMIODeviceManager::restore(restore_args, &device_states).unwrap();

        let expected_vm_resources = format!(
            r#"{{
  "balloon": {{
    "amount_mib": 123,
    "deflate_on_oom": false,
    "stats_polling_interval_s": 1
  }},
  "drives": [
    {{
      "drive_id": "root",
      "path_on_host": "{}",
      "is_root_device": true,
      "partuuid": null,
      "is_read_only": true,
      "cache_type": "Unsafe",
      "rate_limiter": null,
      "io_engine": "Sync"
    }}
  ],
  "boot-source": {{
    "kernel_image_path": "",
    "initrd_path": null
  }},
  "logger": null,
  "machine-config": {{
    "vcpu_count": 1,
    "mem_size_mib": 128,
    "smt": false,
    "track_dirty_pages": false
  }},
  "metrics": null,
  "mmds-config": {{
    "version": "V2",
    "network_interfaces": [
      "netif"
    ],
    "ipv4_address": "169.254.169.254"
  }},
  "network-interfaces": [
    {{
      "iface_id": "netif",
      "host_dev_name": "hostname",
      "guest_mac": "00:00:00:00:00:00",
      "rx_rate_limiter": null,
      "tx_rate_limiter": null
    }}
  ],
  "vsock": {{
    "guest_cid": 3,
    "uds_path": "{}"
  }}
}}"#,
            _block_files
                .last()
                .unwrap()
                .as_path()
                .to_str()
                .unwrap()
                .to_string(),
            tmp_sock_file.as_path().to_str().unwrap().to_string()
        );

        assert_eq!(
            vm_resources
                .mmds
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .version(),
            MmdsVersion::V2
        );
        assert_eq!(device_states.mmds_version.unwrap(), MmdsVersion::V2.into());

        assert_eq!(restored_dev_manager, original_mmio_device_manager);
        assert_eq!(
            expected_vm_resources,
            serde_json::to_string_pretty(&VmmConfig::from(&*vm_resources)).unwrap()
        );
    }
}
