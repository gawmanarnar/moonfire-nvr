// This file is part of Moonfire NVR, a security camera digital video recorder.
// Copyright (C) 2016 Scott Lamb <slamb@slamb.org>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// In addition, as a special exception, the copyright holders give
// permission to link the code of portions of this program with the
// OpenSSL library under certain conditions as described in each
// individual source file, and distribute linked combinations including
// the two.
//
// You must obey the GNU General Public License in all respects for all
// of the code used other than OpenSSL. If you modify file(s) with this
// exception, you may extend this exception to your version of the
// file(s), but you are not obligated to do so. If you do not wish to do
// so, delete this exception statement from your version. If you delete
// this exception statement from all source files in the program, then
// also delete it here.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use clock::Clock;
use db::{Camera, Database};
use dir;
use error::Error;
use h264;
use recording;
use std::result::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use stream;
use time;

pub static ROTATE_INTERVAL_SEC: i64 = 60;

/// Common state that can be used by multiple `Streamer` instances.
pub struct Environment<'a, 'b, C, S> where C: 'a + Clock, S: 'a + stream::Stream {
    pub clock: &'a C,
    pub opener: &'a stream::Opener<S>,
    pub db: &'b Arc<Database>,
    pub dir: &'b Arc<dir::SampleFileDir>,
    pub shutdown: &'b Arc<AtomicBool>,
}

pub struct Streamer<'a, C, S> where C: 'a + Clock, S: 'a + stream::Stream {
    shutdown: Arc<AtomicBool>,

    // State below is only used by the thread in Run.
    rotate_offset_sec: i64,
    rotate_interval_sec: i64,
    db: Arc<Database>,
    dir: Arc<dir::SampleFileDir>,
    syncer_channel: dir::SyncerChannel,
    clock: &'a C,
    opener: &'a stream::Opener<S>,
    camera_id: i32,
    short_name: String,
    url: String,
    redacted_url: String,
}

impl<'a, C, S> Streamer<'a, C, S> where C: 'a + Clock, S: 'a + stream::Stream {
    pub fn new<'b>(env: &Environment<'a, 'b, C, S>, syncer_channel: dir::SyncerChannel,
                   camera_id: i32, c: &Camera, rotate_offset_sec: i64,
                   rotate_interval_sec: i64) -> Self {
        Streamer{
            shutdown: env.shutdown.clone(),
            rotate_offset_sec: rotate_offset_sec,
            rotate_interval_sec: rotate_interval_sec,
            db: env.db.clone(),
            dir: env.dir.clone(),
            syncer_channel: syncer_channel,
            clock: env.clock,
            opener: env.opener,
            camera_id: camera_id,
            short_name: c.short_name.to_owned(),
            url: format!("rtsp://{}:{}@{}{}", c.username, c.password, c.host, c.main_rtsp_path),
            redacted_url: format!("rtsp://{}:redacted@{}{}", c.username, c.host, c.main_rtsp_path),
        }
    }

    pub fn short_name(&self) -> &str { &self.short_name }

    pub fn run(&mut self) {
        while !self.shutdown.load(Ordering::SeqCst) {
            if let Err(e) = self.run_once() {
                let sleep_time = time::Duration::seconds(1);
                warn!("{}: sleeping for {:?} after error: {}", self.short_name, sleep_time, e);
                self.clock.sleep(sleep_time);
            }
        }
        info!("{}: shutting down", self.short_name);
    }

    fn run_once(&mut self) -> Result<(), Error> {
        info!("{}: Opening input: {}", self.short_name, self.redacted_url);

        let mut stream = self.opener.open(stream::Source::Rtsp(&self.url))?;
        // TODO: verify time base.
        // TODO: verify width/height.
        let extra_data = stream.get_extra_data()?;
        let video_sample_entry_id =
            self.db.lock().insert_video_sample_entry(extra_data.width, extra_data.height,
                                                     &extra_data.sample_entry)?;
        debug!("{}: video_sample_entry_id={}", self.short_name, video_sample_entry_id);
        let mut seen_key_frame = false;
        let mut rotate = None;
        let mut writer: Option<dir::Writer> = None;
        let mut transformed = Vec::new();
        let mut next_start = None;
        while !self.shutdown.load(Ordering::SeqCst) {
            let pkt = stream.get_next()?;
            let pts = pkt.pts().ok_or_else(|| Error::new("packet with no pts".to_owned()))?;
            if !seen_key_frame && !pkt.is_key() {
                continue;
            } else if !seen_key_frame {
                debug!("{}: have first key frame", self.short_name);
                seen_key_frame = true;
            }
            let frame_realtime = self.clock.get_time();
            if let Some(r) = rotate {
                if frame_realtime.sec > r && pkt.is_key() {
                    let w = writer.take().expect("rotate set implies writer is set");
                    trace!("{}: write on normal rotation", self.short_name);
                    next_start = Some(w.close(Some(pts))?);
                }
            };
            let mut w = match writer {
                Some(w) => w,
                None => {
                    let r = frame_realtime.sec -
                            (frame_realtime.sec % self.rotate_interval_sec) +
                            self.rotate_offset_sec;
                    rotate = Some(
                        if r <= frame_realtime.sec { r + self.rotate_interval_sec } else { r });
                    let local_realtime = recording::Time::new(frame_realtime);

                    self.dir.create_writer(&self.syncer_channel,
                                           next_start.unwrap_or(local_realtime), local_realtime,
                                           self.camera_id, video_sample_entry_id)?
                },
            };
            let orig_data = match pkt.data() {
                Some(d) => d,
                None => return Err(Error::new("packet has no data".to_owned())),
            };
            let transformed_data = if extra_data.need_transform {
                h264::transform_sample_data(orig_data, &mut transformed)?;
                transformed.as_slice()
            } else {
                orig_data
            };
            w.write(transformed_data, pts, pkt.is_key())?;
            writer = Some(w);
        }
        if let Some(w) = writer {
            w.close(None)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clock::{self, Clock};
    use db;
    use error::Error;
    use ffmpeg;
    use ffmpeg::packet::Mut;
    use h264;
    use recording;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::sync::atomic::{AtomicBool, Ordering};
    use stream::{self, Opener, Stream};
    use testutil;
    use time;

    struct ProxyingStream<'a> {
        clock: &'a clock::SimulatedClock,
        inner: stream::FfmpegStream,
        last_duration: time::Duration,
        ts_offset: i64,
        ts_offset_pkts_left: u32,
        pkts_left: u32,
    }

    impl<'a> ProxyingStream<'a> {
        fn new(clock: &'a clock::SimulatedClock, inner: stream::FfmpegStream) -> ProxyingStream {
            ProxyingStream {
                clock: clock,
                inner: inner,
                last_duration: time::Duration::seconds(0),
                ts_offset: 0,
                ts_offset_pkts_left: 0,
                pkts_left: 0,
            }
        }
    }

    impl<'a> Stream for ProxyingStream<'a> {
        fn get_next(&mut self) -> Result<ffmpeg::Packet, ffmpeg::Error> {
            if self.pkts_left == 0 {
                return Err(ffmpeg::Error::Eof);
            }
            self.pkts_left -= 1;

            // Advance clock to when this packet starts.
            self.clock.sleep(self.last_duration);

            let mut pkt = self.inner.get_next()?;

            self.last_duration = time::Duration::nanoseconds(
                pkt.duration() * 1_000_000_000 / recording::TIME_UNITS_PER_SEC);

            if self.ts_offset_pkts_left > 0 {
                self.ts_offset_pkts_left -= 1;
                let old_pts = pkt.pts().unwrap();
                let old_dts = pkt.dts();
                unsafe {
                    let pkt = pkt.as_mut_ptr();
                    (*pkt).pts = old_pts + self.ts_offset;
                    (*pkt).dts = old_dts + self.ts_offset;

                    // In a real rtsp stream, the duration of a packet is not known until the
                    // next packet. ffmpeg's duration is an unreliable estimate.
                    (*pkt).duration = recording::TIME_UNITS_PER_SEC as i32;
                }
            }

            Ok(pkt)
        }

        fn get_extra_data(&self) -> Result<h264::ExtraData, Error> { self.inner.get_extra_data() }
    }

    struct MockOpener<'a> {
        expected_url: String,
        streams: Mutex<Vec<ProxyingStream<'a>>>,
        shutdown: Arc<AtomicBool>,
    }

    impl<'a> stream::Opener<ProxyingStream<'a>> for MockOpener<'a> {
        fn open(&self, src: stream::Source) -> Result<ProxyingStream<'a>, Error> {
            match src {
                stream::Source::Rtsp(url) => assert_eq!(url, &self.expected_url),
                stream::Source::File(_) => panic!("expected rtsp url"),
            };
            let mut l = self.streams.lock().unwrap();
            match l.pop() {
                Some(stream) => {
                    trace!("MockOpener returning next stream");
                    Ok(stream)
                },
                None => {
                    trace!("MockOpener shutting down");
                    self.shutdown.store(true, Ordering::SeqCst);
                    Err(Error::new("done".to_owned()))
                },
            }
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    struct Frame {
        start_90k: i32,
        duration_90k: i32,
        is_key: bool,
    }

    fn get_frames(db: &MutexGuard<db::LockedDatabase>, recording_id: i64) -> Vec<Frame> {
        let rec = db.get_recording(recording_id).unwrap();
        let mut it = recording::SampleIndexIterator::new();
        let mut frames = Vec::new();
        while it.next(&rec.video_index).unwrap() {
            frames.push(Frame{
                start_90k: it.start_90k,
                duration_90k: it.duration_90k,
                is_key: it.is_key,
            });
        }
        frames
    }

    #[test]
    fn basic() {
        testutil::init();
        let clock = clock::SimulatedClock::new();
        clock.sleep(time::Duration::seconds(1430006400));  // 2015-04-26 00:00:00 UTC
        let stream = stream::FFMPEG.open(stream::Source::File("src/testdata/clip.mp4")).unwrap();
        let mut stream = ProxyingStream::new(&clock, stream);
        stream.ts_offset = 180000;  // starting pts of the input should be irrelevant
        stream.ts_offset_pkts_left = u32::max_value();
        stream.pkts_left = u32::max_value();
        let opener = MockOpener{
            expected_url: "rtsp://foo:bar@test-camera/main".to_owned(),
            streams: Mutex::new(vec![stream]),
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        let db = testutil::TestDb::new();
        let env = super::Environment{
            clock: &clock,
            opener: &opener,
            db: &db.db,
            dir: &db.dir,
            shutdown: &opener.shutdown,
        };
        let mut stream;
        {
            let l = db.db.lock();
            let camera = l.cameras_by_id().get(&testutil::TEST_CAMERA_ID).unwrap();
            stream = super::Streamer::new(&env, db.syncer_channel.clone(), testutil::TEST_CAMERA_ID,
                                          camera, 0, 5);
        }
        stream.run();
        assert!(opener.streams.lock().unwrap().is_empty());
        db.syncer_channel.flush();
        let db = db.db.lock();

        // Compare frame-by-frame. Note below that while the rotation is scheduled to happen near
        // 5-second boundaries (such as 2016-04-26 00:00:05), it gets deferred until the next key
        // frame, which in this case is 00:00:07.
        assert_eq!(get_frames(&db, 1), &[
            Frame{start_90k:      0, duration_90k: 90379, is_key:  true},
            Frame{start_90k:  90379, duration_90k: 89884, is_key: false},
            Frame{start_90k: 180263, duration_90k: 89749, is_key: false},
            Frame{start_90k: 270012, duration_90k: 89981, is_key: false},
            Frame{start_90k: 359993, duration_90k: 90055, is_key:  true},
            Frame{start_90k: 450048, duration_90k: 89967, is_key: false},  // pts_time 5.0005333...
            Frame{start_90k: 540015, duration_90k: 90021, is_key: false},
            Frame{start_90k: 630036, duration_90k: 89958, is_key: false},
        ]);
        assert_eq!(get_frames(&db, 2), &[
            Frame{start_90k:      0, duration_90k: 90011, is_key:  true},
            Frame{start_90k:  90011, duration_90k:     0, is_key: false},
        ]);
    }
}