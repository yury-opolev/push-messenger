//! SDP and ICE candidate checks for call frames (spec D14, D15).
//!
//! The server never interprets SDP beyond these checks: it enforces size, line grammar
//! and audio-only media, then relays the text as an opaque string. Error messages never
//! echo the input, so no SDP or candidate reaches a log through an error.

/// The largest offer, answer or renegotiation SDP, in bytes.
pub const MAX_SDP_BYTES: usize = 16 * 1024;
/// The largest ICE candidate line, in bytes.
pub const MAX_CANDIDATE_BYTES: usize = 512;
/// The largest `sdp_mid`, in bytes.
pub const MAX_SDP_MID_BYTES: usize = 32;
/// The largest `username_fragment`, in bytes.
pub const MAX_UFRAG_BYTES: usize = 256;

/// The longest media token kept in [`CallInputError::NotAudio`], in characters.
const MAX_MEDIA_TOKEN_CHARS: usize = 16;

/// Why an SDP or a candidate was refused. Maps to `ERROR_CODE_INVALID`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallInputError {
    #[error("the SDP is empty")]
    Empty,
    #[error("larger than {limit} bytes")]
    TooLarge { limit: usize },
    #[error("not a valid SDP: {0}")]
    NotSdp(&'static str),
    #[error("video is not available")]
    Video,
    /// A media kind other than audio or video. Holds the `m=` media token (at most 16
    /// characters) for the caller's own use; the message does not show it.
    #[error("only audio calls are available")]
    NotAudio(String),
    #[error("not a valid ICE candidate: {0}")]
    BadCandidate(&'static str),
}

/// Checks an offer, answer or renegotiation SDP: at most [`MAX_SDP_BYTES`], lines of
/// `<lowercase letter>=<value>` separated by LF or CRLF (one trailing line break
/// allowed), the first line exactly `v=0`, no control characters but tab, at least one
/// `m=audio` and no other `m=` kind.
pub fn check_sdp(sdp: &str) -> Result<(), CallInputError> {
    if sdp.is_empty() {
        return Err(CallInputError::Empty);
    }
    if sdp.len() > MAX_SDP_BYTES {
        return Err(CallInputError::TooLarge {
            limit: MAX_SDP_BYTES,
        });
    }
    let mut lines: Vec<&str> = sdp.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let mut audio = false;
    for (index, raw) in lines.into_iter().enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let bytes = line.as_bytes();
        if bytes.len() < 2 || !bytes[0].is_ascii_lowercase() || bytes[1] != b'=' {
            return Err(CallInputError::NotSdp("a line is not <letter>=<value>"));
        }
        if line.chars().any(|c| c.is_control() && c != '\t') {
            return Err(CallInputError::NotSdp("a line holds a control character"));
        }
        if index == 0 && line != "v=0" {
            return Err(CallInputError::NotSdp("the first line is not v=0"));
        }
        if let Some(media) = line.strip_prefix("m=") {
            let token = media.split(' ').next().unwrap_or_default();
            match token {
                "audio" => audio = true,
                "video" => return Err(CallInputError::Video),
                other => {
                    return Err(CallInputError::NotAudio(
                        other.chars().take(MAX_MEDIA_TOKEN_CHARS).collect(),
                    ));
                }
            }
        }
    }
    if audio {
        Ok(())
    } else {
        Err(CallInputError::NotSdp("no audio media"))
    }
}

/// The fields of a `CallIce` frame that [`check_candidate`] checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateInput<'a> {
    pub candidate: &'a str,
    pub sdp_mid: Option<&'a str>,
    pub username_fragment: Option<&'a str>,
    pub end_of_candidates: bool,
}

/// Checks a trickled ICE candidate: `candidate` may be empty only with
/// `end_of_candidates`; otherwise it starts with `candidate:`, is printable ASCII
/// (0x20..=0x7e) and at most [`MAX_CANDIDATE_BYTES`]. `sdp_mid` (at most
/// [`MAX_SDP_MID_BYTES`]) and `username_fragment` (at most [`MAX_UFRAG_BYTES`]) are
/// printable ASCII too.
pub fn check_candidate(input: &CandidateInput<'_>) -> Result<(), CallInputError> {
    let candidate = input.candidate;
    if candidate.is_empty() {
        if !input.end_of_candidates {
            return Err(CallInputError::BadCandidate("the candidate is empty"));
        }
    } else {
        if candidate.len() > MAX_CANDIDATE_BYTES {
            return Err(CallInputError::TooLarge {
                limit: MAX_CANDIDATE_BYTES,
            });
        }
        if !candidate.starts_with("candidate:") {
            return Err(CallInputError::BadCandidate(
                "the candidate does not start with candidate:",
            ));
        }
        if !printable(candidate) {
            return Err(CallInputError::BadCandidate(
                "the candidate is not printable ASCII",
            ));
        }
    }
    check_field(
        input.sdp_mid,
        MAX_SDP_MID_BYTES,
        "sdp_mid is not printable ASCII",
    )?;
    check_field(
        input.username_fragment,
        MAX_UFRAG_BYTES,
        "username_fragment is not printable ASCII",
    )
}

fn check_field(
    value: Option<&str>,
    limit: usize,
    not_printable: &'static str,
) -> Result<(), CallInputError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.len() > limit {
        return Err(CallInputError::TooLarge { limit });
    }
    if !printable(value) {
        return Err(CallInputError::BadCandidate(not_printable));
    }
    Ok(())
}

fn printable(text: &str) -> bool {
    text.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An audio-only offer as Chrome writes it, with candidates gathered before sending.
    pub(crate) const CHROME_AUDIO_OFFER: &str = "v=0\r\n\
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
a=extmap-allow-mixed\r\n\
a=msid-semantic: WMS 6f3c0b4e-2a1d-4c47-9a8e-1f2b3c4d5e6f\r\n\
m=audio 54321 UDP/TLS/RTP/SAVPF 111 63 9 0 8 13 110 126\r\n\
c=IN IP4 203.0.113.7\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=candidate:1467250027 1 udp 2122260223 192.168.1.10 54400 typ host generation 0 network-id 1 network-cost 10\r\n\
a=candidate:842163049 1 udp 1686052607 203.0.113.7 54321 typ srflx raddr 192.168.1.10 rport 54400 generation 0 network-id 1 network-cost 10\r\n\
a=candidate:1853887674 1 tcp 1518280447 192.168.1.10 9 typ host tcptype active generation 0 network-id 1 network-cost 10\r\n\
a=ice-ufrag:EsAw\r\n\
a=ice-pwd:bP+XJMM09aR8AiX1jdukzR6Y\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 7B:8B:F0:65:5F:78:E2:51:3B:AC:6F:F3:3F:46:1B:35:DC:B8:5F:64:1A:24:C2:43:F0:A1:58:D0:A1:2C:19:08\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n\
a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n\
a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n\
a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid\r\n\
a=sendrecv\r\n\
a=msid:6f3c0b4e-2a1d-4c47-9a8e-1f2b3c4d5e6f 0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9\r\n\
a=rtcp-mux\r\n\
a=rtcp-rsize\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtcp-fb:111 transport-cc\r\n\
a=fmtp:111 minptime=10;useinbandfec=1\r\n\
a=rtpmap:63 red/48000/2\r\n\
a=fmtp:63 111/111\r\n\
a=rtpmap:9 G722/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:13 CN/8000\r\n\
a=rtpmap:110 telephone-event/48000\r\n\
a=rtpmap:126 telephone-event/8000\r\n\
a=ssrc:3570614608 cname:4TOk42mSjXCkVIa6\r\n\
a=ssrc:3570614608 msid:6f3c0b4e-2a1d-4c47-9a8e-1f2b3c4d5e6f 0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9\r\n";

    /// The fixture padded with `a=x-pad:` lines to exactly `len` bytes.
    fn padded(len: usize) -> String {
        let mut sdp = CHROME_AUDIO_OFFER.to_owned();
        assert!(len >= sdp.len() + 20);
        while len - sdp.len() > 220 {
            sdp.push_str(&format!("a=x-pad:{}\r\n", "p".repeat(190)));
        }
        let room = len - sdp.len();
        sdp.push_str(&format!("a=x-pad:{}\r\n", "p".repeat(room - 10)));
        assert_eq!(sdp.len(), len);
        sdp
    }

    fn candidate(text: &str) -> CandidateInput<'_> {
        CandidateInput {
            candidate: text,
            sdp_mid: Some("0"),
            username_fragment: Some("EsAw"),
            end_of_candidates: false,
        }
    }

    #[test]
    fn a_chrome_audio_offer_passes() {
        assert_eq!(check_sdp(CHROME_AUDIO_OFFER), Ok(()));
        assert_eq!(check_sdp(&CHROME_AUDIO_OFFER.replace("\r\n", "\n")), Ok(()));
        let unterminated = CHROME_AUDIO_OFFER.strip_suffix("\r\n").unwrap();
        assert_eq!(check_sdp(unterminated), Ok(()));
    }

    #[test]
    fn the_size_limit_is_exact() {
        assert_eq!(check_sdp(&padded(MAX_SDP_BYTES)), Ok(()));
        assert_eq!(
            check_sdp(&padded(MAX_SDP_BYTES + 1)),
            Err(CallInputError::TooLarge {
                limit: MAX_SDP_BYTES
            })
        );
        assert_eq!(check_sdp(""), Err(CallInputError::Empty));
    }

    #[test]
    fn only_audio_media_is_accepted() {
        let video = format!("{CHROME_AUDIO_OFFER}m=video 9 UDP/TLS/RTP/SAVPF 96\r\n");
        assert_eq!(check_sdp(&video), Err(CallInputError::Video));
        let data =
            format!("{CHROME_AUDIO_OFFER}m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n");
        assert_eq!(
            check_sdp(&data),
            Err(CallInputError::NotAudio("application".into()))
        );
        let long = format!("{CHROME_AUDIO_OFFER}m=averyveryverylongmediakind 9 RTP/AVP 0\r\n");
        assert_eq!(
            check_sdp(&long),
            Err(CallInputError::NotAudio("averyveryverylon".into()))
        );
        let none = CHROME_AUDIO_OFFER.replace("m=audio", "a=audio");
        assert_eq!(
            check_sdp(&none),
            Err(CallInputError::NotSdp("no audio media"))
        );
    }

    #[test]
    fn malformed_lines_are_refused() {
        let nul = CHROME_AUDIO_OFFER.replace("s=-", "s=\0");
        assert!(matches!(check_sdp(&nul), Err(CallInputError::NotSdp(_))));
        let first = CHROME_AUDIO_OFFER.replacen("v=0", "v=1", 1);
        assert_eq!(
            check_sdp(&first),
            Err(CallInputError::NotSdp("the first line is not v=0"))
        );
        for bad in [
            "v=0\r\n\r\nm=audio 9 RTP/AVP 0\r\n",
            "v=0\r\nM=audio 9 RTP/AVP 0\r\n",
            "v=0\r\nm audio 9 RTP/AVP 0\r\n",
            "v=0\r\nm=audio 9 RTP/AVP 0\r\n\r\n",
            "v=0\rm=audio 9 RTP/AVP 0",
            " v=0\r\nm=audio 9 RTP/AVP 0\r\n",
        ] {
            assert!(
                matches!(check_sdp(bad), Err(CallInputError::NotSdp(_))),
                "{bad:?}"
            );
        }
        assert_eq!(check_sdp("v=0\nm=audio 9 RTP/AVP 0\ns=a\tb"), Ok(()));
    }

    #[test]
    fn errors_never_echo_the_input() {
        let secret = format!("{CHROME_AUDIO_OFFER}m=secretkind 9 RTP/AVP 0\r\n");
        let message = check_sdp(&secret).unwrap_err().to_string();
        assert!(!message.contains("secret"), "{message}");
        let message = check_candidate(&candidate("candidate:secret\u{1}"))
            .unwrap_err()
            .to_string();
        assert!(!message.contains("secret"), "{message}");
    }

    #[test]
    fn candidates_are_checked() {
        let host = "candidate:1467250027 1 udp 2122260223 192.168.1.10 54400 typ host";
        assert_eq!(check_candidate(&candidate(host)), Ok(()));
        let end = CandidateInput {
            candidate: "",
            sdp_mid: None,
            username_fragment: None,
            end_of_candidates: true,
        };
        assert_eq!(check_candidate(&end), Ok(()));
        for bad in [
            "",
            "a=candidate:1 1 udp 1 192.168.1.10 9 typ host",
            "candidate:1 1 udp 1 192.168.1.10 9 typ host\u{e9}",
            "candidate:1\n1",
        ] {
            assert!(
                matches!(
                    check_candidate(&candidate(bad)),
                    Err(CallInputError::BadCandidate(_))
                ),
                "{bad:?}"
            );
        }
        let longest = format!("candidate:{}", "1".repeat(MAX_CANDIDATE_BYTES - 10));
        assert_eq!(check_candidate(&candidate(&longest)), Ok(()));
        let longer = format!("{longest}1");
        assert_eq!(
            check_candidate(&candidate(&longer)),
            Err(CallInputError::TooLarge {
                limit: MAX_CANDIDATE_BYTES
            })
        );
    }

    #[test]
    fn candidate_metadata_is_bounded_and_printable() {
        let host = "candidate:1 1 udp 1 192.168.1.10 9 typ host";
        let mid = "m".repeat(MAX_SDP_MID_BYTES);
        let ufrag = "u".repeat(MAX_UFRAG_BYTES);
        let ok = CandidateInput {
            sdp_mid: Some(&mid),
            username_fragment: Some(&ufrag),
            ..candidate(host)
        };
        assert_eq!(check_candidate(&ok), Ok(()));
        let long_mid = format!("{mid}m");
        assert_eq!(
            check_candidate(&CandidateInput {
                sdp_mid: Some(&long_mid),
                ..ok
            }),
            Err(CallInputError::TooLarge {
                limit: MAX_SDP_MID_BYTES
            })
        );
        let long_ufrag = format!("{ufrag}u");
        assert_eq!(
            check_candidate(&CandidateInput {
                username_fragment: Some(&long_ufrag),
                ..ok
            }),
            Err(CallInputError::TooLarge {
                limit: MAX_UFRAG_BYTES
            })
        );
        assert!(matches!(
            check_candidate(&CandidateInput {
                sdp_mid: Some("0\t"),
                ..ok
            }),
            Err(CallInputError::BadCandidate(_))
        ));
        assert!(matches!(
            check_candidate(&CandidateInput {
                username_fragment: Some("\u{7f}"),
                ..ok
            }),
            Err(CallInputError::BadCandidate(_))
        ));
    }

    proptest::proptest! {
        #[test]
        fn sdp_check_never_panics(sdp in "(?s).{0,20000}") {
            let _ = check_sdp(&sdp);
        }

        #[test]
        fn sdp_with_a_video_line_is_refused(at in 0usize..64, port in proptest::prelude::any::<u16>()) {
            let mut lines: Vec<String> =
                CHROME_AUDIO_OFFER.split("\r\n").map(str::to_owned).collect();
            let at = at.min(lines.len() - 1);
            lines.insert(at, format!("m=video {port} UDP/TLS/RTP/SAVPF 96 97"));
            let sdp = lines.join("\r\n");
            proptest::prop_assert!(check_sdp(&sdp).is_err());
            if at > 0 {
                proptest::prop_assert_eq!(check_sdp(&sdp), Err(CallInputError::Video));
            }
        }

        #[test]
        fn sdp_padded_beyond_the_limit_is_refused(len in (MAX_SDP_BYTES + 1)..20000) {
            proptest::prop_assert_eq!(
                check_sdp(&padded(len)),
                Err(CallInputError::TooLarge { limit: MAX_SDP_BYTES })
            );
        }

        #[test]
        fn ice_check_never_panics(
            text in "(?s).{0,600}",
            mid in proptest::option::of("(?s).{0,40}"),
            ufrag in proptest::option::of("(?s).{0,300}"),
            end in proptest::prelude::any::<bool>(),
        ) {
            let _ = check_candidate(&CandidateInput {
                candidate: &text,
                sdp_mid: mid.as_deref(),
                username_fragment: ufrag.as_deref(),
                end_of_candidates: end,
            });
        }
    }
}
