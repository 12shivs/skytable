/*
 * Created on Tue Jan 30 2024
 *
 * This file is a part of Skytable
 * Skytable (formerly known as TerrabaseDB or Skybase) is a free and open-source
 * NoSQL database written by Sayan Nandan ("the Author") with the
 * vision to provide flexibility in data modelling without compromising
 * on performance, queryability or scalability.
 *
 * Copyright (c) 2024, Sayan Nandan <nandansayan@outlook.com>
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program. If not, see <https://www.gnu.org/licenses/>.
 *
*/

mod compaction;
mod journal_ops;
mod recovery;

use {
    super::{
        create_journal, open_journal, CommitPreference, DriverEvent, DriverEventKind,
        JournalHeuristics, JournalInitializer, JournalSettings, RawJournalAdapter,
        RawJournalAdapterEvent, RawJournalWriter,
    },
    crate::engine::{
        error::StorageError,
        storage::{
            common::{checksum::SCrc64, sdss::sdss_r1::rw::TrackedReader},
            v2::raw::{
                journal::raw::{JournalReaderTraceEvent, JournalWriterTraceEvent},
                spec::SystemDatabaseV1,
            },
        },
        RuntimeResult,
    },
    std::cell::RefCell,
};

const SANE_MEM_LIMIT_BYTES: usize = 2048;

/*
    impls for journal tests
*/

#[derive(Debug, Clone, PartialEq)]
pub struct SimpleDB {
    data: RefCell<Vec<String>>,
}
impl SimpleDB {
    fn new() -> Self {
        Self {
            data: RefCell::default(),
        }
    }
    fn data(&self) -> std::cell::Ref<'_, Vec<String>> {
        self.data.borrow()
    }
    fn clear(&mut self, log: &mut RawJournalWriter<SimpleDBJournal>) -> RuntimeResult<()> {
        log.commit_event(DbEventClear)?;
        self.data.get_mut().clear();
        Ok(())
    }
    fn pop(&mut self, log: &mut RawJournalWriter<SimpleDBJournal>) -> RuntimeResult<()> {
        self.data.get_mut().pop().unwrap();
        log.commit_event(DbEventPop)?;
        Ok(())
    }
    fn push(
        &mut self,
        log: &mut RawJournalWriter<SimpleDBJournal>,
        new: impl ToString,
    ) -> RuntimeResult<()> {
        let new = new.to_string();
        log.commit_event(DbEventPush(&new))?;
        self.data.get_mut().push(new);
        Ok(())
    }
}

/*
    event impls
*/

#[derive(Debug)]
pub struct SimpleDBJournal;
struct DbEventPush<'a>(&'a str);
struct DbEventPop;
struct DbEventClear;
trait SimpleDBEvent: Sized {
    const OPC: u8;
    fn write_buffered(self, _: &mut Vec<u8>);
}
macro_rules! impl_db_event {
    ($($ty:ty as $code:expr $(=> $expr:expr)?),*) => {
        $(impl SimpleDBEvent for $ty {
            const OPC: u8 = $code;
            fn write_buffered(self, buf: &mut Vec<u8>) { let _ = buf; fn _do_it(s: $ty, b: &mut Vec<u8>, f: impl Fn($ty, &mut Vec<u8>)) { f(s, b) } $(_do_it(self, buf, $expr))? }
        })*
    }
}

impl_db_event!(
    DbEventPush<'_> as 0 => |me, buf| {
        let length_bytes = (me.0.len() as u64).to_le_bytes();
        let me_bytes = me.0.as_bytes();
        let mut checksum = SCrc64::new();
        checksum.update(&length_bytes);
        checksum.update(&me_bytes);
        buf.extend(&(checksum.finish().to_le_bytes())); // checksum
        buf.extend(&length_bytes); // length
        buf.extend(me.0.as_bytes()); // payload
    },
    DbEventPop as 1,
    DbEventClear as 2
);

impl<T: SimpleDBEvent> RawJournalAdapterEvent<SimpleDBJournal> for T {
    fn md(&self) -> u64 {
        T::OPC as _
    }
    fn write_buffered(self, buf: &mut Vec<u8>, _: ()) {
        T::write_buffered(self, buf)
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum EventMeta {
    NewKey,
    Pop,
    Clear,
}
impl RawJournalAdapter for SimpleDBJournal {
    const COMMIT_PREFERENCE: CommitPreference = CommitPreference::Buffered;
    type Spec = SystemDatabaseV1;
    type GlobalState = SimpleDB;
    type EventMeta = EventMeta;
    type CommitContext = ();
    type Context<'a> = () where Self: 'a;
    type FullSyncCtx<'a> = &'a Self::GlobalState;
    fn rewrite_full_journal<'a>(
        writer: &mut RawJournalWriter<Self>,
        full_ctx: Self::FullSyncCtx<'a>,
    ) -> RuntimeResult<()> {
        for key in full_ctx.data().iter() {
            writer.commit_event(DbEventPush(&key))?;
        }
        Ok(())
    }
    fn initialize(_: &JournalInitializer) -> Self {
        Self
    }
    fn enter_context<'a>(_: &'a mut RawJournalWriter<Self>) -> Self::Context<'a> {}
    fn parse_event_meta(meta: u64) -> Option<Self::EventMeta> {
        Some(match meta {
            0 => EventMeta::NewKey,
            1 => EventMeta::Pop,
            2 => EventMeta::Clear,
            _ => return None,
        })
    }
    fn commit_buffered<'a, E: RawJournalAdapterEvent<Self>>(
        &mut self,
        buf: &mut Vec<u8>,
        event: E,
        ctx: (),
    ) {
        event.write_buffered(buf, ctx)
    }
    fn decode_apply<'a>(
        gs: &Self::GlobalState,
        meta: Self::EventMeta,
        file: &mut TrackedReader<Self::Spec>,
        _: &mut JournalHeuristics,
    ) -> RuntimeResult<()> {
        match meta {
            EventMeta::NewKey => {
                let checksum = u64::from_le_bytes(file.read_block()?);
                let length_u64 = u64::from_le_bytes(file.read_block()?);
                let length = length_u64 as usize;
                let mut payload = Vec::<u8>::new();
                if length > SANE_MEM_LIMIT_BYTES
                    || payload.try_reserve_exact(length as usize).is_err()
                {
                    return Err(StorageError::RawJournalDecodeEventCorruptedPayload.into());
                }
                unsafe {
                    payload.as_mut_ptr().write_bytes(0, length);
                    payload.set_len(length);
                }
                file.tracked_read(&mut payload)?;
                let mut this_checksum = SCrc64::new();
                this_checksum.update(&length_u64.to_le_bytes());
                this_checksum.update(&payload);
                match String::from_utf8(payload) {
                    Ok(k) if this_checksum.finish() == checksum => gs.data.borrow_mut().push(k),
                    Err(_) | Ok(_) => {
                        return Err(StorageError::RawJournalDecodeEventCorruptedPayload.into())
                    }
                }
            }
            EventMeta::Clear => gs.data.borrow_mut().clear(),
            EventMeta::Pop => {
                let _ = gs.data.borrow_mut().pop().unwrap();
            }
        }
        Ok(())
    }
}

/*
    basic tests
*/

#[test]
fn encode_decode_meta() {
    let dv1 = DriverEvent::new(u128::MAX - 1, DriverEventKind::Reopened, 0, 0, 0);
    let encoded1 = dv1.encode_self();
    let decoded1 = DriverEvent::decode(encoded1).unwrap();
    assert_eq!(dv1, decoded1);
}

#[test]
fn first_triplet_sanity() {
    // first driver event
    {
        assert_eq!(
            super::debug_get_first_meta_triplet(),
            None,
            "failed for first driver event"
        );
        let mut jrnl = create_journal::<SimpleDBJournal>("first_triplet_sanity_drv_event").unwrap();
        assert_eq!(
            super::debug_get_first_meta_triplet(),
            None,
            "failed for first driver event"
        );
        RawJournalWriter::close_driver(&mut jrnl).unwrap();
        assert_eq!(
            super::debug_get_first_meta_triplet(),
            Some((0, 0, 0)),
            "failed for first driver event"
        );
    }
    // first server event
    {
        assert_eq!(
            super::debug_get_first_meta_triplet(),
            None,
            "failed for first server event"
        );
        let mut jrnl =
            create_journal::<SimpleDBJournal>("first_triplet_sanity_server_event").unwrap();
        assert_eq!(
            super::debug_get_first_meta_triplet(),
            None,
            "failed for first server event"
        );
        SimpleDB::new().push(&mut jrnl, "hello").unwrap();
        assert_eq!(
            super::debug_get_first_meta_triplet(),
            Some((0, 0, 0)),
            "failed for first driver event"
        );
    }
}

#[test]
fn jw_state() {
    let state_backup;
    {
        // open
        let mut jrnl = create_journal::<SimpleDBJournal>("jw_state_test").unwrap();
        jrnl.commit_event(DbEventPush("hello")).unwrap();
        // backup + destroy
        state_backup = jrnl.cleanup().unwrap();
        assert_eq!(
            super::debug_get_trace(),
            intovec![
                JournalWriterTraceEvent::Initialized,
                JournalWriterTraceEvent::CommitAttemptForEvent(0),
                JournalWriterTraceEvent::CommitServerEventWroteMetadata,
                JournalWriterTraceEvent::CommitServerEventAdapterCompleted,
                JournalWriterTraceEvent::CommitCommitServerEventSyncCompleted,
            ]
        );
    }
    {
        // reopen using backup
        let mut jrnl =
            RawJournalWriter::load_using_backup(SimpleDBJournal, "jw_state_test", state_backup)
                .unwrap();
        // write event + close
        jrnl.commit_event(DbEventPush("world")).unwrap();
        RawJournalWriter::close_driver(&mut jrnl).unwrap();
        assert_eq!(
            super::debug_get_trace(),
            intovec![
                JournalWriterTraceEvent::CommitAttemptForEvent(1),
                JournalWriterTraceEvent::CommitServerEventWroteMetadata,
                JournalWriterTraceEvent::CommitServerEventAdapterCompleted,
                JournalWriterTraceEvent::CommitCommitServerEventSyncCompleted,
                JournalWriterTraceEvent::DriverEventAttemptCommit {
                    event: DriverEventKind::Closed,
                    event_id: 2,
                    prev_id: 1
                },
                JournalWriterTraceEvent::DriverEventCompleted,
                JournalWriterTraceEvent::DriverClosed,
            ]
        );
    }
    // reopen and verify
    let db = SimpleDB::new();
    let _ =
        open_journal::<SimpleDBJournal>("jw_state_test", &db, JournalSettings::default()).unwrap();
    assert_eq!(db.data().as_ref(), ["hello", "world"]);
    assert_eq!(
        super::debug_get_trace(),
        into_array![
            // the first time
            JournalReaderTraceEvent::Initialized,
            JournalReaderTraceEvent::LookingForEvent,
            JournalReaderTraceEvent::AttemptingEvent(0),
            JournalReaderTraceEvent::DetectedServerEvent,
            JournalReaderTraceEvent::ServerEventMetadataParsed,
            JournalReaderTraceEvent::ServerEventAppliedSuccess,
            // we closed after this and reopened to apply a server event
            JournalReaderTraceEvent::LookingForEvent,
            JournalReaderTraceEvent::AttemptingEvent(1),
            JournalReaderTraceEvent::DetectedServerEvent,
            JournalReaderTraceEvent::ServerEventMetadataParsed,
            JournalReaderTraceEvent::ServerEventAppliedSuccess,
            JournalReaderTraceEvent::LookingForEvent,
            // we finally closed the log
            JournalReaderTraceEvent::AttemptingEvent(2),
            JournalReaderTraceEvent::DriverEventExpectingClose,
            JournalReaderTraceEvent::DriverEventCompletedBlockRead,
            JournalReaderTraceEvent::DriverEventExpectedCloseGotClose,
            JournalReaderTraceEvent::ClosedAndReachedEof,
            JournalReaderTraceEvent::Completed,
            JournalWriterTraceEvent::ReinitializeAttempt,
            JournalWriterTraceEvent::DriverEventAttemptCommit {
                event: DriverEventKind::Reopened,
                event_id: 3,
                prev_id: 2
            },
            JournalWriterTraceEvent::DriverEventCompleted,
            JournalWriterTraceEvent::ReinitializeComplete,
        ]
    );
}
