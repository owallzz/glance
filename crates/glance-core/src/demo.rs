//! Fake data for building and screenshotting the UI before networking lands.
//! Run the app with `--demo` to start inside a populated room.

use crate::model::*;

fn id(n: u8) -> PeerId {
    PeerId([n; 32])
}

fn profile(n: u8, name: &str, color: u32) -> Profile {
    Profile {
        id: id(n),
        name: name.into(),
        color,
    }
}

pub fn me() -> Profile {
    profile(1, "You", 0x5865f2)
}

pub fn participants() -> Vec<Participant> {
    let base = |p: Profile| Participant {
        profile: p,
        muted: false,
        deafened: false,
        speaking: false,
        camera: false,
        stream: None,
        connection: Some(ConnectionInfo {
            route: Route::Direct,
            rtt_ms: 18,
        }),
        is_contact: true,
    };
    vec![
        Participant {
            connection: None,
            ..base(me())
        },
        Participant {
            speaking: true,
            stream: Some(StreamInfo {
                width: 2560,
                height: 1440,
                fps: 60,
                has_audio: true,
                source: "Screen 1".into(),
            }),
            ..base(profile(2, "Sam Rivera", 0xe67e22))
        },
        Participant {
            muted: true,
            stream: Some(StreamInfo {
                width: 1920,
                height: 1080,
                fps: 30,
                has_audio: false,
                source: "Visual Studio Code".into(),
            }),
            connection: Some(ConnectionInfo {
                route: Route::Relayed,
                rtt_ms: 74,
            }),
            ..base(profile(3, "Ana Kim", 0x1abc9c))
        },
        Participant {
            camera: true,
            is_contact: false,
            ..base(profile(4, "jules", 0xe91e63))
        },
    ]
}

pub fn contacts() -> Vec<Contact> {
    let c = |p: Profile, state, presence| Contact {
        profile: p,
        state,
        nickname: None,
        favorite: false,
        presence,
        in_call: None,
        joinable: false,
    };
    vec![
        Contact {
            favorite: true,
            in_call: Some(3),
            joinable: true,
            ..c(
                profile(2, "Sam Rivera", 0xe67e22),
                ContactState::Mutual,
                Presence::Online,
            )
        },
        c(
            profile(3, "Ana Kim", 0x1abc9c),
            ContactState::Mutual,
            Presence::Idle,
        ),
        c(
            profile(5, "Mo", 0x9b59b6),
            ContactState::Mutual,
            Presence::DoNotDisturb,
        ),
        c(
            profile(6, "Priya Natarajan", 0x3498db),
            ContactState::Mutual,
            Presence::Offline,
        ),
        c(
            profile(7, "Leo", 0xf1c40f),
            ContactState::Mutual,
            Presence::Online,
        ),
        c(
            profile(4, "jules", 0xe91e63),
            ContactState::Incoming,
            Presence::Online,
        ),
        c(
            profile(8, "Kai", 0x2ecc71),
            ContactState::Outgoing,
            Presence::Offline,
        ),
    ]
}

pub fn chat() -> Vec<ChatMessage> {
    let m = |n: u64, author: u8, body| ChatMessage {
        id: n,
        author: id(author),
        sent_at: 1_700_000_000_000 + n * 60_000,
        body,
    };
    vec![
        m(
            1,
            2,
            ChatBody::System("Sam Rivera started streaming".into()),
        ),
        m(
            2,
            2,
            ChatBody::Text("can everyone see the build output?".into()),
        ),
        m(3, 3, ChatBody::Text("yep, sharing my editor too".into())),
        m(
            4,
            3,
            ChatBody::File(FileOffer {
                hash: [9; 32],
                name: "crash-report.log".into(),
                size: 182_400,
                mime: "text/plain".into(),
                transfer: Transfer::Available,
            }),
        ),
        m(
            5,
            2,
            ChatBody::File(FileOffer {
                hash: [10; 32],
                name: "recording-final.mp4".into(),
                size: 1_840_000_000,
                mime: "video/mp4".into(),
                transfer: Transfer::Downloading {
                    done: 620_000_000,
                    bytes_per_sec: 48_000_000,
                },
            }),
        ),
        m(
            6,
            3,
            ChatBody::Text(
                "found it: the decoder panics when a stream starts **mid-GOP**\n\
                 ```rs\n\
                 fn decode(&mut self, pkt: &Packet) -> Result<Frame> {\n    \
                 // wait for a keyframe first\n    \
                 if !self.synced && !pkt.keyframe {\n        \
                 return Err(Error::NeedKeyframe);\n    \
                 }\n    \
                 self.synced = true;\n    \
                 self.inner.decode(&pkt.data)\n\
                 }\n\
                 ```"
                .into(),
            ),
        ),
        m(
            7,
            4,
            ChatBody::Text(
                "> wait for a keyframe first\n\
                 doesn't `BroadcastControl` already send one on join? *genuinely asking*"
                    .into(),
            ),
        ),
        m(
            8,
            2,
            ChatBody::Text(
                "### release checklist\n\
                 - bump the version\n\
                 - tag and push\n  \
                 - wait for CI on __all three__ platforms\n\
                 - write the notes\n\
                 -# steps are in the [release guide](https://example.com/releasing)"
                    .into(),
            ),
        ),
        m(
            9,
            1,
            ChatBody::Text(
                "looks good, ship it ~~tomorrow~~ today. ||the fix was a one-liner all along||"
                    .into(),
            ),
        ),
    ]
}

pub fn recent_calls() -> Vec<CallRecord> {
    vec![
        CallRecord {
            with: vec![id(2), id(3)],
            started_at: 1_700_000_000_000,
            duration_secs: 3_725,
            missed: false,
        },
        CallRecord {
            with: vec![id(5)],
            started_at: 1_699_900_000_000,
            duration_secs: 0,
            missed: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_room_has_you_first_and_streams() {
        let ps = participants();
        assert!(ps[0].connection.is_none());
        assert!(ps.iter().filter(|p| p.stream.is_some()).count() >= 2);
    }
}
