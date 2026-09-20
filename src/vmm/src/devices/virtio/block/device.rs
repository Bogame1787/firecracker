// Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use event_manager::{EventOps, Events, MutEventSubscriber};
use log::info;
use vmm_sys_util::eventfd::EventFd;

use super::BlockError;
use super::persist::{BlockConstructorArgs, BlockState};
use super::vhost_user::device::{VhostUserBlock, VhostUserBlockConfig};
use super::virtio::device::{VirtioBlock, VirtioBlockConfig};
use crate::devices::virtio::ActivateError;
use crate::devices::virtio::device::{VirtioDevice, VirtioDeviceType};
use crate::devices::virtio::queue::{InvalidAvailIdx, Queue};
use crate::devices::virtio::transport::VirtioInterrupt;
use crate::impl_device_type;
use crate::rate_limiter::BucketUpdate;
use crate::snapshot::Persist;
use crate::vmm_config::drive::BlockDeviceConfig;
use crate::vstate::memory::GuestMemoryMmap;

// Clippy thinks that values of the enum are too different in size.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Block {
    Virtio(VirtioBlock),
    VhostUser(VhostUserBlock),
}

impl Block {
    pub fn new(config: BlockDeviceConfig) -> Result<Block, BlockError> {
        if let Ok(config) = VirtioBlockConfig::try_from(&config) {
            Ok(Self::Virtio(
                VirtioBlock::new(config).map_err(BlockError::VirtioBackend)?,
            ))
        } else if let Ok(config) = VhostUserBlockConfig::try_from(&config) {
            Ok(Self::VhostUser(
                VhostUserBlock::new(config).map_err(BlockError::VhostUserBackend)?,
            ))
        } else {
            Err(BlockError::InvalidBlockConfig)
        }
    }

    pub fn config(&self) -> BlockDeviceConfig {
        match self {
            Self::Virtio(b) => b.config().into(),
            Self::VhostUser(b) => b.config().into(),
        }
    }

    pub fn update_disk_image(&mut self, disk_image_path: String) -> Result<(), BlockError> {
        match self {
            Self::Virtio(b) => b
                .update_disk_image(disk_image_path)
                .map_err(BlockError::VirtioBackend),
            Self::VhostUser(_) => Err(BlockError::InvalidBlockBackend),
        }
    }

    /// see VirtioBlock::prepare_reset
    pub fn prepare_reset(&mut self) -> Result<(), BlockError> {
        match self {
            Self::Virtio(b) => b.prepare_reset().map_err(BlockError::VirtioBackend),
            Self::VhostUser(_) => Err(BlockError::InvalidBlockBackend),
        }
    }

    /// see VirtioBlock::reset_to_state. vhost-user isn't supported.
    pub fn reset_to_state(
        &mut self,
        args: BlockConstructorArgs,
        state: &BlockState,
    ) -> Result<(), BlockError> {
        match (self, state) {
            (Self::Virtio(b), BlockState::Virtio(s)) => {
                b.reset_to_state(args, s).map_err(BlockError::VirtioBackend)
            }
            _ => Err(BlockError::InvalidBlockBackend),
        }
    }

    pub fn update_rate_limiter(
        &mut self,
        bytes: BucketUpdate,
        ops: BucketUpdate,
    ) -> Result<(), BlockError> {
        match self {
            Self::Virtio(b) => {
                b.update_rate_limiter(bytes, ops);
                Ok(())
            }
            Self::VhostUser(_) => Err(BlockError::InvalidBlockBackend),
        }
    }

    pub fn update_config(&mut self) -> Result<(), BlockError> {
        match self {
            Self::Virtio(_) => Err(BlockError::InvalidBlockBackend),
            Self::VhostUser(b) => b.config_update().map_err(BlockError::VhostUserBackend),
        }
    }

    pub fn process_virtio_queues(&mut self) -> Result<(), InvalidAvailIdx> {
        match self {
            Self::Virtio(b) => b.process_virtio_queues(),
            Self::VhostUser(_) => Ok(()),
        }
    }

    pub fn root_device(&self) -> bool {
        match self {
            Self::Virtio(b) => b.root_device,
            Self::VhostUser(b) => b.root_device,
        }
    }

    pub fn read_only(&self) -> bool {
        match self {
            Self::Virtio(b) => b.read_only,
            Self::VhostUser(b) => b.read_only,
        }
    }

    pub fn partuuid(&self) -> &Option<String> {
        match self {
            Self::Virtio(b) => &b.partuuid,
            Self::VhostUser(b) => &b.partuuid,
        }
    }

    pub fn is_vhost_user(&self) -> bool {
        match self {
            Self::Virtio(_) => false,
            Self::VhostUser(_) => true,
        }
    }
}

impl VirtioDevice for Block {
    impl_device_type!(VirtioDeviceType::Block);

    fn id(&self) -> &str {
        match self {
            Self::Virtio(b) => b.id(),
            Self::VhostUser(b) => b.id(),
        }
    }

    fn avail_features(&self) -> u64 {
        match self {
            Self::Virtio(b) => b.avail_features,
            Self::VhostUser(b) => b.avail_features,
        }
    }

    fn acked_features(&self) -> u64 {
        match self {
            Self::Virtio(b) => b.acked_features,
            Self::VhostUser(b) => b.acked_features,
        }
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        match self {
            Self::Virtio(b) => b.acked_features = acked_features,
            Self::VhostUser(b) => b.acked_features = acked_features,
        }
    }

    fn queues(&self) -> &[Queue] {
        match self {
            Self::Virtio(b) => &b.queues,
            Self::VhostUser(b) => &b.queues,
        }
    }

    fn queues_mut(&mut self) -> &mut [Queue] {
        match self {
            Self::Virtio(b) => &mut b.queues,
            Self::VhostUser(b) => &mut b.queues,
        }
    }

    fn queue_events(&self) -> &[EventFd] {
        match self {
            Self::Virtio(b) => &b.queue_evts,
            Self::VhostUser(b) => &b.queue_evts,
        }
    }

    fn interrupt_trigger(&self) -> &dyn VirtioInterrupt {
        match self {
            Self::Virtio(b) => b.interrupt_trigger(),
            Self::VhostUser(b) => b.interrupt_trigger(),
        }
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        match self {
            Self::Virtio(b) => b.read_config(offset, data),
            Self::VhostUser(b) => b.read_config(offset, data),
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        match self {
            Self::Virtio(b) => b.write_config(offset, data),
            Self::VhostUser(b) => b.write_config(offset, data),
        }
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: Arc<dyn VirtioInterrupt>,
    ) -> Result<(), ActivateError> {
        match self {
            Self::Virtio(b) => b.activate(mem, interrupt),
            Self::VhostUser(b) => b.activate(mem, interrupt),
        }
    }

    fn is_activated(&self) -> bool {
        match self {
            Self::Virtio(b) => b.device_state.is_activated(),
            Self::VhostUser(b) => b.device_state.is_activated(),
        }
    }

    fn prepare_save(&mut self) {
        match self {
            Self::Virtio(b) => b.prepare_save(),
            Self::VhostUser(b) => b.prepare_save(),
        }
    }
}

impl MutEventSubscriber for Block {
    fn process(&mut self, event: Events, ops: &mut EventOps) {
        match self {
            Self::Virtio(b) => b.process(event, ops),
            Self::VhostUser(b) => b.process(event, ops),
        }
    }

    fn init(&mut self, ops: &mut EventOps) {
        match self {
            Self::Virtio(b) => b.init(ops),
            Self::VhostUser(b) => b.init(ops),
        }
    }
}

impl Persist<'_> for Block {
    type State = BlockState;
    type ConstructorArgs = BlockConstructorArgs;
    type Error = BlockError;

    fn save(&self) -> Self::State {
        match self {
            Self::Virtio(b) => BlockState::Virtio(b.save()),
            Self::VhostUser(b) => BlockState::VhostUser(b.save()),
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> Result<Self, Self::Error> {
        match state {
            BlockState::Virtio(s) => Ok(Self::Virtio(
                VirtioBlock::restore(constructor_args, s).map_err(BlockError::VirtioBackend)?,
            )),
            BlockState::VhostUser(s) => Ok(Self::VhostUser(
                VhostUserBlock::restore(constructor_args, s)
                    .map_err(BlockError::VhostUserBackend)?,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::devices::virtio::block::virtio::device::FileEngineType;
    use crate::devices::virtio::block::virtio::test_utils::default_block_with_path;
    use crate::devices::virtio::test_utils::{VirtQueue, default_interrupt, default_mem};
    use vm_memory::GuestAddress;

    #[test]
    fn test_block_reset_dispatches_to_virtio_backend() {
        let f = TempFile::new().unwrap();
        f.as_file().set_len(0x1000).unwrap();
        let mut inner = default_block_with_path(
            f.as_path().to_str().unwrap().to_string(),
            FileEngineType::Sync,
        );

        let mem = default_mem();
        let vq = VirtQueue::new(GuestAddress(0), &mem, 256);
        inner.queues[0] = vq.create_queue();
        inner.activate(mem.clone(), default_interrupt()).unwrap();

        let mut block = Block::Virtio(inner);
        let state = block.save();
        assert!(matches!(state, BlockState::Virtio(_)));

        block.prepare_reset().unwrap();
        block.reset_to_state(BlockConstructorArgs { mem }, &state).unwrap();

        match &block {
            Block::Virtio(b) => assert!(b.is_activated()),
            Block::VhostUser(_) => panic!("variant changed across reset"),
        }
    }
}
