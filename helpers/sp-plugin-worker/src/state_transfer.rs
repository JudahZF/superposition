//! Bounded, single-transfer staging for opaque worker plug-in state.

use sp_protocol::payload::{MAX_STATE_STREAM_BYTES, MAX_STATE_TRANSFER_CHUNK_BYTES};

use crate::worker_runtime::PluginState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferDescriptor {
    pub id: u64,
    pub component_len: u32,
    pub controller_len: u32,
}

enum TransferContents {
    Capture(PluginState),
    Restore {
        expected_component: usize,
        expected_controller: usize,
        state: PluginState,
    },
}

struct Transfer {
    id: u64,
    slot: usize,
    contents: TransferContents,
}

/// At most one snapshot or staged restore belongs to a worker at a time.
pub struct StateTransfers {
    next_id: u64,
    current: Option<Transfer>,
}

impl Default for StateTransfers {
    fn default() -> Self {
        Self {
            next_id: 1,
            current: None,
        }
    }
}

impl StateTransfers {
    /// Starts a transfer ID space. Disjoint spaces let one worker route IDs to their owner.
    pub const fn starting_at(first_id: u64) -> Self {
        Self {
            next_id: first_id,
            current: None,
        }
    }

    pub fn clear(&mut self) {
        self.current = None;
    }

    pub fn holds_slot(&self, slot: usize) -> bool {
        self.current
            .as_ref()
            .is_some_and(|transfer| transfer.slot == slot)
    }

    pub fn begin_capture(
        &mut self,
        slot: usize,
        state: PluginState,
    ) -> Result<TransferDescriptor, String> {
        self.clear();
        let component_len = checked_len(state.component.len())?;
        let controller_len = checked_len(state.controller.len())?;
        let id = self.next_id()?;
        self.current = Some(Transfer {
            id,
            slot,
            contents: TransferContents::Capture(state),
        });
        Ok(TransferDescriptor {
            id,
            component_len,
            controller_len,
        })
    }

    pub fn read_chunk(
        &self,
        slot: usize,
        id: u64,
        stream: u32,
        offset: u32,
        length: u32,
    ) -> Result<Vec<u8>, String> {
        let transfer = self.matching(slot, id)?;
        let TransferContents::Capture(state) = &transfer.contents else {
            return Err("state transfer is not a capture".to_owned());
        };
        let source = select_stream(state, stream)?;
        let length = usize::try_from(length).map_err(|_| "chunk length overflow")?;
        if length == 0 || length > MAX_STATE_TRANSFER_CHUNK_BYTES {
            return Err("state chunk length is outside the bounded range".to_owned());
        }
        let start = usize::try_from(offset).map_err(|_| "state offset overflow")?;
        let end = start
            .checked_add(length)
            .ok_or("state chunk offset overflow")?;
        let bytes = source
            .get(start..end)
            .ok_or("state chunk exceeds captured stream")?;
        Ok(bytes.to_vec())
    }

    pub fn begin_restore(
        &mut self,
        slot: usize,
        component_len: u32,
        controller_len: u32,
    ) -> Result<TransferDescriptor, String> {
        self.clear();
        let expected_component =
            checked_len(usize::try_from(component_len).map_err(|_| "component length overflow")?)?;
        let expected_controller = checked_len(
            usize::try_from(controller_len).map_err(|_| "controller length overflow")?,
        )?;
        let mut component = Vec::new();
        component
            .try_reserve_exact(expected_component as usize)
            .map_err(|error| format!("could not reserve component state: {error}"))?;
        let mut controller = Vec::new();
        controller
            .try_reserve_exact(expected_controller as usize)
            .map_err(|error| format!("could not reserve controller state: {error}"))?;
        let id = self.next_id()?;
        self.current = Some(Transfer {
            id,
            slot,
            contents: TransferContents::Restore {
                expected_component: expected_component as usize,
                expected_controller: expected_controller as usize,
                state: PluginState {
                    component,
                    controller,
                },
            },
        });
        Ok(TransferDescriptor {
            id,
            component_len,
            controller_len,
        })
    }

    pub fn write_chunk(
        &mut self,
        slot: usize,
        id: u64,
        stream: u32,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), String> {
        if bytes.is_empty() || bytes.len() > MAX_STATE_TRANSFER_CHUNK_BYTES {
            return Err("state chunk length is outside the bounded range".to_owned());
        }
        let transfer = self.matching_mut(slot, id)?;
        let TransferContents::Restore {
            expected_component,
            expected_controller,
            state,
        } = &mut transfer.contents
        else {
            return Err("state transfer is not a restore".to_owned());
        };
        let (target, expected) = match stream {
            0 => (&mut state.component, *expected_component),
            1 => (&mut state.controller, *expected_controller),
            _ => return Err("unknown state stream".to_owned()),
        };
        let offset = usize::try_from(offset).map_err(|_| "state offset overflow")?;
        if offset != target.len() {
            return Err("state chunks must be contiguous".to_owned());
        }
        if target
            .len()
            .checked_add(bytes.len())
            .is_none_or(|end| end > expected)
        {
            return Err("state chunk exceeds declared stream length".to_owned());
        }
        target.extend_from_slice(bytes);
        Ok(())
    }

    pub fn commit_restore(&mut self, slot: usize, id: u64) -> Result<PluginState, String> {
        let transfer = self.matching(slot, id)?;
        let TransferContents::Restore {
            expected_component,
            expected_controller,
            state,
        } = &transfer.contents
        else {
            return Err("state transfer is not a restore".to_owned());
        };
        if state.component.len() != *expected_component
            || state.controller.len() != *expected_controller
        {
            return Err("state restore is incomplete".to_owned());
        }
        let Some(Transfer {
            contents: TransferContents::Restore { state, .. },
            ..
        }) = self.current.take()
        else {
            unreachable!("matching restore was checked above")
        };
        Ok(state)
    }

    pub fn release(&mut self, slot: usize, id: u64) -> Result<(), String> {
        self.matching(slot, id)?;
        self.clear();
        Ok(())
    }

    fn matching(&self, slot: usize, id: u64) -> Result<&Transfer, String> {
        self.current
            .as_ref()
            .filter(|transfer| transfer.slot == slot && transfer.id == id)
            .ok_or("state transfer ID or slot does not match".to_owned())
    }

    fn matching_mut(&mut self, slot: usize, id: u64) -> Result<&mut Transfer, String> {
        self.current
            .as_mut()
            .filter(|transfer| transfer.slot == slot && transfer.id == id)
            .ok_or("state transfer ID or slot does not match".to_owned())
    }

    fn next_id(&mut self) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or("state transfer ID space is exhausted")?;
        Ok(id)
    }
}

fn checked_len(length: usize) -> Result<u32, String> {
    if length > MAX_STATE_STREAM_BYTES {
        return Err("state stream exceeds the 32 MiB session limit".to_owned());
    }
    u32::try_from(length).map_err(|_| "state stream length overflow".to_owned())
}

fn select_stream(state: &PluginState, stream: u32) -> Result<&[u8], String> {
    match stream {
        0 => Ok(&state.component),
        1 => Ok(&state.controller),
        _ => Err("unknown state stream".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multimegabyte_roundtrip_is_bounded_and_complete_before_commit() {
        let original = PluginState {
            component: vec![0x5a; 3 * 1024 * 1024 + 17],
            controller: vec![0x93; 1024 * 1024 + 3],
        };
        let mut transfers = StateTransfers::default();
        let captured = transfers
            .begin_capture(2, original.clone())
            .expect("capture");
        let mut downloaded = PluginState::default();
        for (stream, output, length) in [
            (0, &mut downloaded.component, captured.component_len),
            (1, &mut downloaded.controller, captured.controller_len),
        ] {
            while output.len() < length as usize {
                let remaining = length as usize - output.len();
                let chunk = transfers
                    .read_chunk(
                        2,
                        captured.id,
                        stream,
                        u32::try_from(output.len()).expect("bounded output length"),
                        u32::try_from(remaining.min(MAX_STATE_TRANSFER_CHUNK_BYTES))
                            .expect("bounded chunk length"),
                    )
                    .expect("read chunk");
                output.extend(chunk);
            }
        }
        assert_eq!(downloaded, original);
        let staged = transfers
            .begin_restore(2, captured.component_len, captured.controller_len)
            .expect("begin restore");
        assert!(transfers.commit_restore(2, staged.id).is_err());
        for (stream, source) in [(0, &original.component), (1, &original.controller)] {
            for (index, chunk) in source.chunks(MAX_STATE_TRANSFER_CHUNK_BYTES).enumerate() {
                transfers
                    .write_chunk(
                        2,
                        staged.id,
                        stream,
                        u32::try_from(index * MAX_STATE_TRANSFER_CHUNK_BYTES)
                            .expect("bounded state offset"),
                        chunk,
                    )
                    .expect("write chunk");
            }
        }
        assert_eq!(transfers.commit_restore(2, staged.id).unwrap(), original);
    }

    #[test]
    fn stale_slot_and_out_of_order_chunks_cannot_mutate_staging() {
        let mut transfers = StateTransfers::default();
        let descriptor = transfers.begin_restore(1, 4, 0).expect("begin restore");
        assert!(
            transfers
                .write_chunk(2, descriptor.id, 0, 0, b"ab")
                .is_err()
        );
        assert!(
            transfers
                .write_chunk(1, descriptor.id, 0, 2, b"ab")
                .is_err()
        );
        assert!(
            transfers
                .write_chunk(1, descriptor.id + 1, 0, 0, b"ab")
                .is_err()
        );
        assert!(transfers.commit_restore(1, descriptor.id).is_err());
        transfers
            .write_chunk(1, descriptor.id, 0, 0, b"abcd")
            .expect("contiguous chunk");
        assert_eq!(
            transfers
                .commit_restore(1, descriptor.id)
                .unwrap()
                .component,
            b"abcd"
        );
    }
}
