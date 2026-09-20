// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Defines the structures needed for saving/restoring block devices.

use device::ConfigSpace;
use serde::{Deserialize, Serialize};
use vmm_sys_util::eventfd::EventFd;

use super::device::DiskProperties;
use super::*;
use crate::devices::virtio::block::persist::BlockConstructorArgs;
use crate::devices::virtio::block::virtio::device::FileEngineType;
use crate::devices::virtio::block::virtio::metrics::BlockMetricsPerDevice;
use crate::devices::virtio::device::{ActiveState, DeviceState, VirtioDeviceType};
use crate::devices::virtio::generated::virtio_blk::VIRTIO_BLK_F_RO;
use crate::devices::virtio::persist::VirtioDeviceState;
use crate::rate_limiter::RateLimiter;
use crate::rate_limiter::persist::RateLimiterState;
use crate::snapshot::Persist;

/// Holds info about block's file engine type. Gets saved in snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileEngineTypeState {
    /// Sync File Engine.
    // If the snap version does not contain the `FileEngineType`, it must have been snapshotted
    // on a VM using the Sync backend.
    #[default]
    Sync,
    /// Async File Engine.
    Async,
}

impl From<FileEngineType> for FileEngineTypeState {
    fn from(file_engine_type: FileEngineType) -> Self {
        match file_engine_type {
            FileEngineType::Sync => FileEngineTypeState::Sync,
            FileEngineType::Async => FileEngineTypeState::Async,
        }
    }
}

impl From<FileEngineTypeState> for FileEngineType {
    fn from(file_engine_type_state: FileEngineTypeState) -> Self {
        match file_engine_type_state {
            FileEngineTypeState::Sync => FileEngineType::Sync,
            FileEngineTypeState::Async => FileEngineType::Async,
        }
    }
}

/// Holds info about the block device. Gets saved in snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtioBlockState {
    id: String,
    partuuid: Option<String>,
    cache_type: CacheType,
    root_device: bool,
    disk_path: String,
    pub virtio_state: VirtioDeviceState,
    rate_limiter_state: RateLimiterState,
    file_engine_type: FileEngineTypeState,
}

impl Persist<'_> for VirtioBlock {
    type State = VirtioBlockState;
    type ConstructorArgs = BlockConstructorArgs;
    type Error = VirtioBlockError;

    fn save(&self) -> Self::State {
        // Save device state.
        VirtioBlockState {
            id: self.id.clone(),
            partuuid: self.partuuid.clone(),
            cache_type: self.cache_type,
            root_device: self.root_device,
            disk_path: self.disk.file_path.clone(),
            virtio_state: VirtioDeviceState::from_device(self),
            rate_limiter_state: self.rate_limiter.save(),
            file_engine_type: FileEngineTypeState::from(self.file_engine_type()),
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> Result<Self, Self::Error> {
        let is_read_only = state.virtio_state.avail_features & (1u64 << VIRTIO_BLK_F_RO) != 0;
        let rate_limiter = RateLimiter::restore((), &state.rate_limiter_state)
            .map_err(VirtioBlockError::RateLimiter)?;

        let disk_properties = DiskProperties::new(
            state.disk_path.clone(),
            is_read_only,
            state.file_engine_type.into(),
        )?;

        let queue_evts = [EventFd::new(libc::EFD_NONBLOCK).map_err(VirtioBlockError::EventFd)?];

        let queues = state
            .virtio_state
            .build_queues_checked(
                &constructor_args.mem,
                VirtioDeviceType::Block,
                BLOCK_NUM_QUEUES,
                FIRECRACKER_MAX_QUEUE_SIZE,
            )
            .map_err(VirtioBlockError::Persist)?;

        let avail_features = state.virtio_state.avail_features;
        let acked_features = state.virtio_state.acked_features;

        let config_space = ConfigSpace {
            capacity: disk_properties.nsectors.to_le(),
        };

        Ok(VirtioBlock {
            avail_features,
            acked_features,
            config_space,
            activate_evt: EventFd::new(libc::EFD_NONBLOCK).map_err(VirtioBlockError::EventFd)?,

            queues,
            queue_evts,
            device_state: DeviceState::Inactive,

            id: state.id.clone(),
            partuuid: state.partuuid.clone(),
            cache_type: state.cache_type,
            root_device: state.root_device,
            read_only: is_read_only,

            disk: disk_properties,
            rate_limiter,
            is_io_engine_throttled: false,
            metrics: BlockMetricsPerDevice::alloc(state.id.clone()),
        })
    }
}

impl VirtioBlock {
    /// apply a saved state to the live device. keeps the fd, engine, eventfds and
    /// interrupt; everything else comes from the snapshot. run after guest memory
    /// has been reverted since the queues get rebound to it.
    pub fn reset_to_state(
        &mut self,
        args: BlockConstructorArgs,
        state: &VirtioBlockState,
    ) -> Result<(), VirtioBlockError> {
        // same backing file or bail, we don't reopen
        if self.disk.file_path != state.disk_path {
            return Err(VirtioBlockError::DiskPathMismatch {
                current: self.disk.file_path.clone(),
                target: state.disk_path.clone(),
            });
        }
        let target_engine: FileEngineType = state.file_engine_type.into();
        if self.file_engine_type() != target_engine {
            return Err(VirtioBlockError::FileEngineTypeMismatch {
                current: self.file_engine_type(),
                target: target_engine,
            });
        }

        // keep the interrupt, it's wired into kvm
        let interrupt = self
            .device_state
            .active_state()
            .map(|active| active.interrupt.clone());

        self.rate_limiter
            .reset(&state.rate_limiter_state)
            .map_err(VirtioBlockError::RateLimiter)?;

        self.queues = state
            .virtio_state
            .build_queues_checked(
                &args.mem,
                VirtioDeviceType::Block,
                BLOCK_NUM_QUEUES,
                FIRECRACKER_MAX_QUEUE_SIZE,
            )
            .map_err(VirtioBlockError::Persist)?;

        self.avail_features = state.virtio_state.avail_features;
        self.acked_features = state.virtio_state.acked_features;

        self.id = state.id.clone();
        self.partuuid = state.partuuid.clone();
        self.cache_type = state.cache_type;
        self.root_device = state.root_device;
        self.read_only = state.virtio_state.avail_features & (1u64 << VIRTIO_BLK_F_RO) != 0;
        self.is_io_engine_throttled = false;

        // todo: will need to come from the snapshot once the backing file can change
        self.config_space = ConfigSpace {
            capacity: self.disk.nsectors.to_le(),
        };

        if state.virtio_state.activated {
            self.apply_activation_state(&args.mem)?;

            match interrupt {
                Some(interrupt) => {
                    self.device_state = DeviceState::Activated(ActiveState {
                        mem: args.mem,
                        interrupt,
                    });
                }
                None => {
                    // nothing to reinstate
                    log::warn!(
                        "block reset: target state for {} is activated but the live device \
                         had no interrupt; leaving device inactive",
                        self.id
                    );
                    self.device_state = DeviceState::Inactive;
                }
            }
        } else {
            self.device_state = DeviceState::Inactive;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::devices::virtio::block::virtio::device::VirtioBlockConfig;
    use crate::devices::virtio::device::VirtioDevice;
    use crate::devices::virtio::test_utils::{default_interrupt, default_mem};
    use crate::vstate::memory::GuestMemoryMmap;

    #[test]
    fn test_cache_semantic_ser() {
        // We create the backing file here so that it exists for the whole lifetime of the test.
        let f = TempFile::new().unwrap();
        f.as_file().set_len(0x1000).unwrap();

        let config = VirtioBlockConfig {
            drive_id: "test".to_string(),
            path_on_host: f.as_path().to_str().unwrap().to_string(),
            is_root_device: false,
            partuuid: None,
            is_read_only: false,
            cache_type: CacheType::Writeback,
            rate_limiter: None,
            file_engine_type: FileEngineType::default(),
        };

        let block = VirtioBlock::new(config).unwrap();

        // Save the block device.
        let block_state = block.save();
        let _serialized_data = bitcode::serialize(&block_state).unwrap();
    }

    #[test]
    fn test_file_engine_type() {
        // Test conversions between FileEngineType and FileEngineTypeState.
        assert_eq!(
            FileEngineTypeState::Async,
            FileEngineTypeState::from(FileEngineType::Async)
        );
        assert_eq!(
            FileEngineTypeState::Sync,
            FileEngineTypeState::from(FileEngineType::Sync)
        );
        assert_eq!(FileEngineType::Async, FileEngineTypeState::Async.into());
        assert_eq!(FileEngineType::Sync, FileEngineTypeState::Sync.into());
        // Test default impl.
        assert_eq!(FileEngineTypeState::default(), FileEngineTypeState::Sync);
    }

    #[test]
    fn test_persistence() {
        // We create the backing file here so that it exists for the whole lifetime of the test.
        let f = TempFile::new().unwrap();
        f.as_file().set_len(0x1000).unwrap();

        let config = VirtioBlockConfig {
            drive_id: "test".to_string(),
            path_on_host: f.as_path().to_str().unwrap().to_string(),
            is_root_device: false,
            partuuid: None,
            is_read_only: false,
            cache_type: CacheType::Unsafe,
            rate_limiter: None,
            file_engine_type: FileEngineType::default(),
        };

        let block = VirtioBlock::new(config).unwrap();
        let guest_mem = default_mem();

        // Save the block device.
        let block_state = block.save();
        let serialized_data = bitcode::serialize(&block_state).unwrap();

        // Restore the block device.
        let restored_state = bitcode::deserialize(&serialized_data).unwrap();
        let restored_block =
            VirtioBlock::restore(BlockConstructorArgs { mem: guest_mem }, &restored_state).unwrap();

        // Test that virtio specific fields are the same.
        assert_eq!(restored_block.device_type(), VirtioDeviceType::Block);
        assert_eq!(restored_block.avail_features(), block.avail_features());
        assert_eq!(restored_block.acked_features(), block.acked_features());
        assert_eq!(restored_block.queues(), block.queues());
        assert!(!block.is_activated());
        assert!(!restored_block.is_activated());

        // Test that block specific fields are the same.
        assert_eq!(restored_block.disk.file_path, block.disk.file_path);
    }

    // tempfile has to outlive the device
    fn activated_block(engine: FileEngineType) -> (VirtioBlock, TempFile, GuestMemoryMmap) {
        use std::os::unix::io::AsRawFd;
        use crate::devices::virtio::block::virtio::test_utils::default_block_with_path;
        use crate::devices::virtio::test_utils::VirtQueue;
        use vm_memory::GuestAddress;

        let f = TempFile::new().unwrap();
        f.as_file().set_len(0x1000).unwrap();
        let mut block = default_block_with_path(f.as_path().to_str().unwrap().to_string(), engine);

        let mem = default_mem();
        // build_queues_checked wants max_size == 256
        let vq = VirtQueue::new(GuestAddress(0), &mem, 256);
        block.queues[0] = vq.create_queue();
        block.activate(mem.clone(), default_interrupt()).unwrap();

        assert!(block.is_activated());
        assert!(block.disk.file_engine.file().as_raw_fd() > 0);

        (block, f, mem)
    }

    #[test]
    fn test_reset_rewinds_queue_cursors() {
        use std::num::Wrapping;

        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let (mut block, _f, mem) = activated_block(engine);

            let clean = block.save();
            let clean_avail = block.queues[0].next_avail;
            let clean_used = block.queues[0].next_used;

            // cursors live on the heap, madvise won't touch them
            block.queues[0].next_avail = Wrapping(4217);
            block.queues[0].next_used = Wrapping(4217);
            block.queues[0].num_added = Wrapping(9);

            block.reset_to_state(BlockConstructorArgs { mem: mem.clone() }, &clean).unwrap();

            assert_eq!(block.queues[0].next_avail, clean_avail);
            assert_eq!(block.queues[0].next_used, clean_used);
            assert_eq!(block.queues[0].num_added, Wrapping(0));

            assert!(!block.queues[0].desc_table_ptr.is_null());
            assert!(block.is_activated());
        }
    }

    #[test]
    fn test_reset_preserves_host_resources() {
        use std::os::unix::io::AsRawFd;
        use std::sync::Arc;

        for engine in [FileEngineType::Sync, FileEngineType::Async] {
            let (mut block, _f, mem) = activated_block(engine);
            let clean = block.save();

            let disk_fd = block.disk.file_engine.file().as_raw_fd();
            let queue_evt_fd = block.queue_evts[0].as_raw_fd();
            let activate_evt_fd = block.activate_evt.as_raw_fd();
            let rl_fd = block.rate_limiter.as_raw_fd();
            let metrics_ptr = Arc::as_ptr(&block.metrics);
            let interrupt_ptr = {
                let active = block.device_state.active_state().unwrap();
                Arc::as_ptr(&active.interrupt) as *const () as usize
            };

            block.reset_to_state(BlockConstructorArgs { mem: mem.clone() }, &clean).unwrap();

            // all the same objects
            assert_eq!(block.disk.file_engine.file().as_raw_fd(), disk_fd);
            assert_eq!(block.queue_evts[0].as_raw_fd(), queue_evt_fd);
            assert_eq!(block.activate_evt.as_raw_fd(), activate_evt_fd);
            assert_eq!(block.rate_limiter.as_raw_fd(), rl_fd);
            assert!(std::ptr::eq(Arc::as_ptr(&block.metrics), metrics_ptr));
            let after = {
                let active = block.device_state.active_state().unwrap();
                Arc::as_ptr(&active.interrupt) as *const () as usize
            };
            assert_eq!(after, interrupt_ptr);
        }
    }

    #[test]
    fn test_reset_restores_features_and_flags() {
        let (mut block, _f, mem) = activated_block(FileEngineType::Sync);
        let clean = block.save();
        let clean_acked = block.acked_features;
        let clean_ro = block.read_only;

        block.acked_features = 0;
        block.is_io_engine_throttled = true;
        block.read_only = !clean_ro;
        block.config_space.capacity = 0;
        block.cache_type = CacheType::Writeback;

        block.reset_to_state(BlockConstructorArgs { mem }, &clean).unwrap();

        assert_eq!(block.acked_features, clean_acked);
        assert!(!block.is_io_engine_throttled);
        assert_eq!(block.read_only, clean_ro);
        assert_eq!(block.config_space.capacity, block.disk.nsectors.to_le());
        assert_eq!(block.cache_type, CacheType::Unsafe);
    }

    #[test]
    fn test_reset_disk_path_mismatch() {
        let (mut block, _f, mem) = activated_block(FileEngineType::Sync);
        let mut state = block.save();
        state.disk_path = "/nonexistent/other.img".to_string();

        let err = block.reset_to_state(BlockConstructorArgs { mem }, &state).unwrap_err();
        assert!(matches!(err, VirtioBlockError::DiskPathMismatch { .. }), "{err:?}");
    }

    #[test]
    fn test_reset_engine_type_mismatch() {
        let (mut block, _f, mem) = activated_block(FileEngineType::Sync);
        let mut state = block.save();
        state.file_engine_type = FileEngineTypeState::Async;

        let err = block.reset_to_state(BlockConstructorArgs { mem }, &state).unwrap_err();
        assert!(matches!(err, VirtioBlockError::FileEngineTypeMismatch { .. }), "{err:?}");
    }

    #[test]
    fn test_reset_to_inactive_state() {
        let (mut block, _f, mem) = activated_block(FileEngineType::Sync);
        let mut state = block.save();
        state.virtio_state.activated = false;

        block.reset_to_state(BlockConstructorArgs { mem }, &state).unwrap();
        assert!(!block.is_activated());
    }
}
