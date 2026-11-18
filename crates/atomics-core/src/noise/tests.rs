use super::*;

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn key(text: &str) -> [u8; KEY_BYTES] {
    unhex(text).try_into().unwrap()
}

const HOST_ID: &str = "0190a1b2-c3d4-8e5f-a617-283940516273";
/// RFC 7748 §6.1: Bob is the host, Alice the device.
const HOST: &str = "5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb";
const HOST_PUBLIC: &str = "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f";
const DEVICE: &str = "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a";
const DEVICE_PUBLIC: &str = "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a";
/// The cacophony vectors' ephemerals.
const DEVICE_EPHEMERAL: &str = "893e28b9dc6ca8d611ab664754b8ceb7bac5117349a4439a6b0569da977c464a";
const HOST_EPHEMERAL: &str = "bbdb4cdbd309f1a1f2e1456967fe288cadd6f712d65dc7b7793d5e63da6b375b";
const PSK: &[u8; 32] = b"atomics pairing test vector psk!";

fn host() -> Secret {
    Secret::from_bytes(&key(HOST))
}

fn device() -> Secret {
    Secret::from_bytes(&key(DEVICE))
}

fn pairing() -> Pairing {
    Pairing { host_id: HOST_ID.into(), host_key: host().public(), psk: Secret::from_bytes(PSK) }
}

/// Both ends of a handshake with fresh ephemerals: the device's established channel and code,
/// and the host's.
fn connect(kind: Kind, device: &Secret, host_psk: Option<&Secret>) -> Result<(Established, Established), NoiseError> {
    let (initiator, first) = match kind {
        Kind::Pair => Initiator::pair(&pairing(), device)?,
        Kind::Session => Initiator::connect(HOST_ID, &host().public(), device)?,
    };
    let responder = Responder::read(&first, HOST_ID, &host(), host_psk)?;
    assert_eq!(responder.device(), device.public());
    let (answer, hosted) = responder.answer()?;
    Ok((initiator.finish(&answer)?, hosted))
}

/// One frame through `from` and back out of `to`.
fn carry(from: &mut Channel, to: &mut Channel, frame: &[u8]) -> Vec<u8> {
    let messages = from.seal(frame).unwrap();
    let mut out = None;
    for (at, message) in messages.iter().enumerate() {
        let opened = to.open(message).unwrap();
        assert_eq!(opened.is_some(), at == messages.len() - 1, "a frame is whole only after its last message");
        out = opened;
    }
    out.unwrap()
}

/// The cacophony vectors (as `snow` ships them) for exactly our two protocol names: the library
/// and the names we picked are standard Noise.
#[test]
fn both_protocols_match_the_standard_vectors() {
    let payloads: [&[u8]; 4] = [b"Ludwig von Mises", b"Murray Rothbard", b"F. A. Hayek", b"Carl Menger"];
    let cases = [
        (
            SESSION_PROTOCOL,
            None,
            "0b0f68fb0c27e03ce9b97565995ed4838cc0581b762ef72b062f6a546419fad7",
            [
                "ca35def5ae56cec33dc2036731ab14896bc4c75dbb07a61f879f8e3afa4c7944718da798efbcd91528520204f904b9bd6c7413dccdc214d951e15253e39987f18146e8cd0873654207148333479d4d16c289f0294b29960a72f48e0b7bba2e89083169825e59642148d492020664ccf7",
                "95ebc60d2b1fa672c1f46a8aa265ef51bfe38e7ccb39ec5be34069f1448088435361e70b2ed446e6c9ec387d1d6b3b840f194e373979d241b203c4acafccf5",
                "050e9f3c8fac16b68dbce8f8c4bfbf6617c897f9ada4aa29aa19c8",
                "344233a6cabb7141d80f3da2fedc311d9646bbb0f505afe403a667",
            ],
        ),
        (
            PAIR_PROTOCOL,
            Some(*b"This is my Austrian perspective!"),
            "8310f86394dc0dabb40beb8210031556db4403ab1202db7034c526232147a700",
            [
                "ca35def5ae56cec33dc2036731ab14896bc4c75dbb07a61f879f8e3afa4c79442ec9b09893d0f510791784c10cbc959f25b1766e0def6e301d14fbca1c7790ac829b8b3674f5f649a5f0e98479662cbfbf2b2c47cd4b09fcd266cd29d7cb675f1808849707847840f6d178ec4d3733aa",
                "95ebc60d2b1fa672c1f46a8aa265ef51bfe38e7ccb39ec5be34069f1448088439a1b3cebf680b2c74217fcb5eba4ff58a9468cd90c4aca6194f57479b379a7",
                "a8fde7a0accec190cd306c5950d4fd8e04a205ec288aa747d8b347",
                "59caddd9984a3bbe24c4fb31a2bd455b7eba3fa0980674b1a3a5f9",
            ],
        ),
    ];
    let prologue = b"John Galt";
    let init_static = key("e61ef9919cde45dd5f82166404bd08e38bceb5dfdfded0a34c8df7ed542214d1");
    let resp_static = key("4a3acbfdb163dec651dfa3194dece676d437029c62a408b4c5ea9114246e4893");
    let resp_public = key("31e0303fd6418d2f8c0e78b91f22e8caed0fbe48656dcf4767e4834f701b8f62");
    let (init_e, resp_e) = (key(DEVICE_EPHEMERAL), key(HOST_EPHEMERAL));
    for (protocol, psk, hash, expected) in cases {
        let side = |local: &[u8; 32], ephemeral: &[u8; 32], initiator: bool| {
            let mut builder = snow::Builder::new(protocol.parse().unwrap())
                .prologue(prologue)
                .unwrap()
                .local_private_key(local)
                .unwrap()
                .fixed_ephemeral_key_for_testing_only(ephemeral);
            if initiator {
                builder = builder.remote_public_key(&resp_public).unwrap();
            }
            if let Some(psk) = &psk {
                builder = builder.psk(2, psk).unwrap();
            }
            let state = if initiator { builder.build_initiator() } else { builder.build_responder() };
            state.unwrap()
        };
        let (mut init, mut resp) = (side(&init_static, &init_e, true), side(&resp_static, &resp_e, false));
        let mut buf = [0u8; 1024];
        let mut payload = [0u8; 1024];
        let n = init.write_message(payloads[0], &mut buf).unwrap();
        assert_eq!(hex(&buf[..n]), expected[0], "{protocol}: message 1");
        resp.read_message(&buf[..n], &mut payload).unwrap();
        let n = resp.write_message(payloads[1], &mut buf).unwrap();
        assert_eq!(hex(&buf[..n]), expected[1], "{protocol}: message 2");
        init.read_message(&buf[..n], &mut payload).unwrap();
        assert_eq!(hex(init.get_handshake_hash()), hash);
        assert_eq!(hex(resp.get_handshake_hash()), hash);
        let (mut init, mut resp) = (init.into_transport_mode().unwrap(), resp.into_transport_mode().unwrap());
        let n = init.write_message(payloads[2], &mut buf).unwrap();
        assert_eq!(hex(&buf[..n]), expected[2], "{protocol}: transport, initiator to responder");
        let n = resp.write_message(payloads[3], &mut buf).unwrap();
        assert_eq!(hex(&buf[..n]), expected[3], "{protocol}: transport, responder to initiator");
    }
}

/// Our framing on top of Noise, byte for byte: the kind byte, the prologue, the code and the
/// flags byte. Computed by an independent implementation (Python: RFC 7748, RFC 8439 and the
/// Noise rules, checked against the same standard vectors), so a client written in another
/// language can test itself against these.
#[test]
fn pairing_and_session_vectors() {
    assert_eq!(hex(host().public().as_bytes()), HOST_PUBLIC, "RFC 7748 §6.1");
    assert_eq!(hex(device().public().as_bytes()), DEVICE_PUBLIC);
    assert_eq!(prologue(HOST_ID), format!("atomics-noise-v1\natomics.v1\n1\n{HOST_ID}").into_bytes());
    let cases = [
        (
            Kind::Pair,
            "01ca35def5ae56cec33dc2036731ab14896bc4c75dbb07a61f879f8e3afa4c79440fd01cfb9ae3291e4770fd905deb84503335f99241144105a166585402272363e5778cb43894c4ffd31ac4b40ec72dd616bcb692f6a683b05a5d72429add13b6",
            "95ebc60d2b1fa672c1f46a8aa265ef51bfe38e7ccb39ec5be34069f1448088437c7c3e7c8a31faea9b46f20ab1319d2b",
            "2030B00EFAD3C757B33FA4C7",
            "7a7c4ff1c7ce1d4bbce85b6b4e89105acf7aa6fddec869bf75161dfc519dd2ca8ed0a9e6d708",
            "4047aa27a516a24ff40624e589603fef3af24f12af280f1ba15a881f9207c2c7c441ff76a238a625529f052381bcca59",
        ),
        (
            Kind::Session,
            "02ca35def5ae56cec33dc2036731ab14896bc4c75dbb07a61f879f8e3afa4c7944651ce4d01a9635b17a7dfcc0db0839cca1c8f355c8d1da6deb4b45942db13f4197336b9716bba066647df1055aac50b9afde5da1b65e026a6a76d481ff1ec787",
            "95ebc60d2b1fa672c1f46a8aa265ef51bfe38e7ccb39ec5be34069f144808843e3f80105977c5bb19146ae8832b4cdfb",
            "F37388CDE4140DE9F5962372",
            "a765ea9cca5de7519352da14f5286806d5ce386a1a61c61f963681e77287308f3a8c5d4a3b9e",
            "87283a58032b7ccb707d29d4d0138b529215101056a0a6c01006668e9e85821e0e31db0796758d90cf06dccc8025dfe4",
        ),
    ];
    let psk = Secret::from_bytes(PSK);
    for (kind, first_hex, second_hex, code, request_hex, answer_hex) in cases {
        let host_psk = (kind == Kind::Pair).then_some(&psk);
        let (initiator, first) =
            Initiator::start(kind, HOST_ID, &host().public(), &device(), host_psk, Some(&key(DEVICE_EPHEMERAL)))
                .unwrap();
        assert_eq!(hex(&first), first_hex, "{kind:?}: the first frame");
        assert_eq!(first.len(), FIRST_FRAME_BYTES);
        assert_eq!(Kind::of(&first), Ok(kind));
        let responder = Responder::read_with(&first, HOST_ID, &host(), host_psk, Some(&key(HOST_EPHEMERAL))).unwrap();
        assert_eq!(hex(responder.device().as_bytes()), DEVICE_PUBLIC, "{kind:?}: the device's key, authenticated");
        let (second, mut hosted) = responder.answer().unwrap();
        assert_eq!(hex(&second), second_hex, "{kind:?}: the answer");
        assert_eq!(second.len(), SECOND_FRAME_BYTES);
        let mut browser = initiator.finish(&second).unwrap();
        assert_eq!((browser.code.as_str(), hosted.code.as_str()), (code, code), "{kind:?}: one code on both screens");
        let request = browser.channel.seal(br#"{"name":"Test phone"}"#).unwrap();
        assert_eq!(
            request.iter().map(|m| hex(m)).collect::<Vec<_>>(),
            [request_hex],
            "{kind:?}: flags 0, then the frame"
        );
        assert_eq!(hosted.channel.open(&request[0]).unwrap().unwrap(), br#"{"name":"Test phone"}"#);
        let answer = hosted.channel.seal(br#"{"paired":true,"may":"control"}"#).unwrap();
        assert_eq!(hex(&answer[0]), answer_hex, "{kind:?}: the host's first transport message");
        assert_eq!(browser.channel.open(&answer[0]).unwrap().unwrap(), br#"{"paired":true,"may":"control"}"#);
    }
    assert_eq!(show_code("0123456789ABCDEF01234567"), "0123 4567 89AB\nCDEF 0123 4567");
}

#[test]
fn a_wrong_psk_fails_on_both_ends() {
    let mut wrong = pairing();
    wrong.psk = Secret::from_bytes(&[9; 32]);
    let (initiator, first) = Initiator::pair(&wrong, &device()).unwrap();
    // The host cannot tell yet: message 1 mixes no psk in IKpsk2.
    let responder = Responder::read(&first, HOST_ID, &host(), Some(&Secret::from_bytes(PSK))).unwrap();
    let (answer, mut hosted) = responder.answer().unwrap();
    assert_eq!(initiator.finish(&answer).err(), Some(NoiseError::Refused), "the browser refuses the host's answer");
    // Without the psk nothing the browser sends can be read: the host never shows a prompt.
    assert_eq!(hosted.channel.open(&[0x5a; 40]), Err(NoiseError::Refused));
    let (_, first) = Initiator::pair(&wrong, &device()).unwrap();
    // And a pairing frame with no psk on the host, or a session frame with one, is refused.
    assert_eq!(Responder::read(&first, HOST_ID, &host(), None).err(), Some(NoiseError::Malformed));
    let (_, session) = Initiator::connect(HOST_ID, &host().public(), &device()).unwrap();
    assert_eq!(
        Responder::read(&session, HOST_ID, &host(), Some(&Secret::from_bytes(PSK))).err(),
        Some(NoiseError::Malformed)
    );
}

#[test]
fn a_device_racing_the_link_gets_another_code() {
    // Two devices answer the same link: each handshake has its own ephemerals, so their codes
    // differ, and the host's code matches only the one it completed with.
    let psk = Secret::from_bytes(PSK);
    let (phone, hosted) = connect(Kind::Pair, &device(), Some(&psk)).unwrap();
    assert_eq!(phone.code, hosted.code);
    assert_eq!(phone.code.len(), 24);
    assert!(phone.code.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(show_code(&phone.code).split_whitespace().map(str::len).collect::<Vec<_>>(), vec![4; 6]);
    let codes: std::collections::HashSet<String> =
        (0..8).map(|_| connect(Kind::Pair, &Secret::generate().unwrap(), Some(&psk)).unwrap().0.code).collect();
    assert!(!codes.contains(&hosted.code) && codes.len() >= 7, "{codes:?} {}", hosted.code);
}

#[test]
fn a_replayed_handshake_or_message_fails() {
    // A recorded session replayed into a new channel: the host accepts message 1 (an IK
    // property) but answers with a new ephemeral, so the replayed transport message fails.
    let (initiator, first) = Initiator::connect(HOST_ID, &host().public(), &device()).unwrap();
    let (answer, mut hosted) = Responder::read(&first, HOST_ID, &host(), None).unwrap().answer().unwrap();
    let mut browser = initiator.finish(&answer).unwrap();
    let hello = browser.channel.seal(b"hello").unwrap();
    assert_eq!(hosted.channel.open(&hello[0]).unwrap().unwrap(), b"hello");
    let (_, mut replayed) = Responder::read(&first, HOST_ID, &host(), None).unwrap().answer().unwrap();
    assert_eq!(replayed.channel.open(&hello[0]), Err(NoiseError::Refused), "the old session's keys are gone");
    // Inside a session: the same message twice, or out of order, fails.
    assert_eq!(hosted.channel.open(&hello[0]), Err(NoiseError::Refused), "replayed");
    let (mut browser, mut hosted) = connect(Kind::Session, &device(), None).unwrap();
    let one = browser.channel.seal(b"one").unwrap();
    let two = browser.channel.seal(b"two").unwrap();
    assert_eq!(hosted.channel.open(&two[0]), Err(NoiseError::Refused), "reordered");
    // A tampered message fails too.
    let (mut browser, mut hosted) = connect(Kind::Session, &device(), None).unwrap();
    let mut message = browser.channel.seal(b"approve").unwrap().remove(0);
    message[3] ^= 1;
    assert_eq!(hosted.channel.open(&message), Err(NoiseError::Refused));
    let _ = one;
}

#[test]
fn a_handshake_for_another_host_major_or_key_fails() {
    let (_, first) = Initiator::connect(HOST_ID, &host().public(), &device()).unwrap();
    assert_eq!(
        Responder::read(&first, "0190a1b2-c3d4-8e5f-a617-283940516274", &host(), None).err(),
        Some(NoiseError::Refused),
        "the prologue binds the host id"
    );
    let other = Secret::generate().unwrap();
    assert_eq!(Responder::read(&first, HOST_ID, &other, None).err(), Some(NoiseError::Refused), "another host's key");
    // A frame moved to the other handshake, cut short, or with an unknown kind.
    let mut moved = first.clone();
    moved[0] = 1;
    assert_eq!(
        Responder::read(&moved, HOST_ID, &host(), Some(&Secret::from_bytes(PSK))).err(),
        Some(NoiseError::Refused)
    );
    assert_eq!(Kind::of(&first[..FIRST_FRAME_BYTES - 1]), Err(NoiseError::Malformed));
    assert_eq!(Kind::of(&[first.clone(), vec![0]].concat()), Err(NoiseError::Malformed));
    let mut unknown = first;
    unknown[0] = 3;
    assert_eq!(Kind::of(&unknown), Err(NoiseError::Malformed));
    assert_eq!(Kind::of(br#"{"kind":"hello"}"#), Err(NoiseError::Malformed), "no JSON before the handshake");
    assert_eq!(Initiator::connect("bad id\n", &host().public(), &device()).err(), Some(NoiseError::Malformed));
    // A short or forged answer.
    let (initiator, _) = Initiator::connect(HOST_ID, &host().public(), &device()).unwrap();
    assert_eq!(initiator.finish(&[0; SECOND_FRAME_BYTES]).err(), Some(NoiseError::Refused));
    let (initiator, _) = Initiator::connect(HOST_ID, &host().public(), &device()).unwrap();
    assert_eq!(initiator.finish(&[0; 10]).err(), Some(NoiseError::Malformed));
}

#[test]
fn frames_travel_in_messages_within_noise_limits_and_rekey() {
    let (mut browser, mut hosted) = connect(Kind::Session, &device(), None).unwrap();
    let big: Vec<u8> = (0..wire::MAX_FRAME_BYTES).map(|i| (i % 251) as u8).collect();
    let messages = browser.channel.seal(&big).unwrap();
    assert_eq!(messages.len(), wire::MAX_FRAME_BYTES.div_ceil(MAX_CHUNK_BYTES));
    assert!(messages.iter().all(|m| m.len() <= MAX_MESSAGE_BYTES), "Noise's own limit");
    let mut whole = None;
    for message in &messages {
        whole = hosted.channel.open(message).unwrap();
    }
    assert_eq!(whole.unwrap(), big);
    assert_eq!(carry(&mut hosted.channel, &mut browser.channel, b""), b"", "an empty frame is one message");
    assert_eq!(browser.channel.seal(&vec![0; wire::MAX_FRAME_BYTES + 1]).err(), Some(NoiseError::TooBig));
    // A peer that never ends its frame is cut off at the frame limit.
    let mut flood = Vec::new();
    for _ in 0..=wire::MAX_FRAME_BYTES / MAX_CHUNK_BYTES {
        let mut plain = vec![MORE];
        plain.extend(std::iter::repeat_n(7u8, MAX_CHUNK_BYTES));
        let mut message = vec![0u8; plain.len() + TAG_BYTES];
        let n = browser.channel.state.write_message(&plain, &mut message).unwrap();
        flood.push(message[..n].to_vec());
    }
    let last = flood.len() - 1;
    for (at, message) in flood.iter().enumerate() {
        let opened = hosted.channel.open(message);
        if at < last {
            assert_eq!(opened, Ok(None));
        } else {
            assert_eq!(opened, Err(NoiseError::TooBig));
        }
    }
    // Unknown flags are refused.
    let (mut browser, mut hosted) = connect(Kind::Session, &device(), None).unwrap();
    let mut message = vec![0u8; 1 + TAG_BYTES];
    let n = browser.channel.state.write_message(&[0x80], &mut message).unwrap();
    assert_eq!(hosted.channel.open(&message[..n]), Err(NoiseError::Malformed));

    // Rekeying: by bytes, and when asked; each side follows the other's flag.
    let (mut browser, mut hosted) = connect(Kind::Session, &device(), None).unwrap();
    browser.channel.rekey_every(10);
    hosted.channel.rekey_soon();
    for round in 0..5 {
        let frame = format!("frame {round} with more than ten bytes");
        assert_eq!(carry(&mut browser.channel, &mut hosted.channel, frame.as_bytes()), frame.as_bytes());
        assert_eq!(carry(&mut hosted.channel, &mut browser.channel, frame.as_bytes()), frame.as_bytes());
    }
    // The flag really changes the key: a reader that skips it cannot read on.
    let (mut browser, mut hosted) = connect(Kind::Session, &device(), None).unwrap();
    browser.channel.rekey_soon();
    let first = browser.channel.seal(b"x").unwrap().remove(0);
    let second = browser.channel.seal(b"y").unwrap().remove(0);
    let mut plain = [0u8; 64];
    let n = hosted.channel.state.read_message(&first, &mut plain).unwrap();
    assert_eq!(&plain[..n], [REKEY, b'x'], "flagged, and read without following the flag");
    assert_eq!(hosted.channel.open(&second), Err(NoiseError::Refused));
    let (mut browser, mut hosted) = connect(Kind::Session, &device(), None).unwrap();
    browser.channel.rekey_soon();
    let first = browser.channel.seal(b"x").unwrap().remove(0);
    let second = browser.channel.seal(b"y").unwrap().remove(0);
    assert_eq!(hosted.channel.open(&first).unwrap().unwrap(), b"x");
    assert_eq!(hosted.channel.open(&second).unwrap().unwrap(), b"y", "both ends moved to the new key");
}

#[test]
fn pairing_links_are_read_strictly_and_never_show_the_psk() {
    let pairing = pairing();
    let fragment = pairing.fragment();
    let parts: Vec<&str> = fragment.split('.').collect();
    assert_eq!((parts[0], parts[1], parts.len()), ("1", HOST_ID, 4));
    assert_eq!((parts[2].len(), parts[3].len()), (43, 43));
    assert_eq!(parts[2], host().public().text());
    let read = Pairing::read(&format!("#{}", *fragment)).unwrap();
    assert_eq!((read.host_id.as_str(), read.host_key), (HOST_ID, host().public()));
    assert_eq!(read.psk().as_bytes(), PSK);
    assert!(!format!("{read:?}").contains(parts[3]) && format!("{read:?}").contains(HOST_ID));
    assert!(!format!("{:?}", read.psk()).contains(parts[3]));
    let newer = format!("2.{HOST_ID}.{}.{}", parts[2], parts[3]);
    assert_eq!(Pairing::read(&newer).err(), Some(LinkProblem::Newer));
    // A non-canonical key: the same 32 bytes with a spare bit set.
    let mut odd = parts[2].to_string();
    let last = odd.pop().unwrap();
    let index = ALPHABET.iter().position(|&a| a as char == last).unwrap();
    odd.push(ALPHABET[index | 1] as char);
    for bad in [
        String::new(),
        "1".into(),
        format!("01.{HOST_ID}.{}.{}", parts[2], parts[3]),
        format!("1.{HOST_ID}.{}", parts[2]),
        format!("1.{HOST_ID}.{}.{}.x", parts[2], parts[3]),
        format!("1.{HOST_ID}.{odd}.{}", parts[3]),
        format!("1.{HOST_ID}.{}.{}=", parts[2], parts[3]),
        format!("1.{HOST_ID}.+{}.{}", &parts[2][1..], parts[3]),
        format!("1.bad id.{}.{}", parts[2], parts[3]),
        format!(" 1.{HOST_ID}.{}.{}", parts[2], parts[3]),
    ] {
        assert!(Pairing::read(&bad).is_err(), "{bad:?}");
    }
    assert_eq!(PublicKey::parse(&host().public().text()), Some(host().public()));
    let json = serde_json::to_string(&host().public()).unwrap();
    assert_eq!(serde_json::from_str::<PublicKey>(&json).unwrap(), host().public());
    assert!(serde_json::from_str::<PublicKey>("\"short\"").is_err());
    // A fresh link has a fresh psk.
    let a = Pairing::new(HOST_ID, host().public()).unwrap();
    let b = Pairing::new(HOST_ID, host().public()).unwrap();
    assert_ne!(a.psk().as_bytes(), b.psk().as_bytes());
    assert_eq!(Pairing::new("no spaces", host().public()).err(), Some(NoiseError::Malformed));
}

#[test]
fn pair_url_templates_keep_the_secret_in_the_fragment() {
    let pairing = pairing();
    let link = pairing.link("https://service.example/atomics/pair#{pairing}").unwrap();
    assert_eq!(*link, format!("https://service.example/atomics/pair#{}", *pairing.fragment()));
    assert!(pairing.link("http://localhost:8080/pair#{pairing}").is_ok(), "a workstation preview");
    for bad in [
        "",
        "https://service.example/pair",
        "https://service.example/pair?p={pairing}",
        "https://service.example/{pairing}#{pairing}",
        "https://service.example/pair#x{pairing}",
        "https://service.example/pair#{pairing}#{pairing}",
        "http://service.example/pair#{pairing}",
        "http://localhost.example/pair#{pairing}",
        "ftp://service.example/pair#{pairing}",
    ] {
        assert!(pair_url_problem(bad).is_some(), "{bad:?}");
        assert!(pairing.link(bad).is_err());
    }
}

#[test]
fn device_names_and_access() {
    assert_eq!(device_name("  Phone\u{7}  "), "Phone");
    assert_eq!(device_name(""), "Unnamed device");
    assert_eq!(device_name(&"x".repeat(200)).chars().count(), MAX_DEVICE_NAME);
    assert_eq!(Access::View.role(), Role::Viewer);
    assert_eq!(Access::Control.role(), Role::Controller);
    assert_eq!(
        serde_json::to_string(&PairAnswer { paired: true, may: Some(Access::View) }).unwrap(),
        r#"{"paired":true,"may":"view"}"#
    );
    assert_eq!(serde_json::to_string(&PairAnswer { paired: false, may: None }).unwrap(), r#"{"paired":false}"#);
    assert_eq!(
        serde_json::from_str::<PairRequest>(r#"{"name":"Phone"}"#).unwrap(),
        PairRequest { name: "Phone".into() }
    );
}
