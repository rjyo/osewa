//! vtfilter_test.go

use crate::vtfilter::{printable_lines, FState, QueryFilter};

type Case = (&'static str, Vec<u8>, Vec<u8>);

fn filter_cases() -> Vec<Case> {
    vec![
        ("plain", "hello\r\nworld".as_bytes().to_vec(), "hello\r\nworld".as_bytes().to_vec()),
        ("utf8", "你好 😀 é ñ".as_bytes().to_vec(), "你好 😀 é ñ".as_bytes().to_vec()),
        ("sgr", "\x1b[1;31mred\x1b[0m".as_bytes().to_vec(), "\x1b[1;31mred\x1b[0m".as_bytes().to_vec()),
        ("cursor", "\x1b[5;3H\x1b[2J\x1b[?1049h\x1b[?25l".as_bytes().to_vec(), "\x1b[5;3H\x1b[2J\x1b[?1049h\x1b[?25l".as_bytes().to_vec()),
        ("dsr status", "a\x1b[5nb".as_bytes().to_vec(), "ab".as_bytes().to_vec()),
        ("dsr cursor", "a\x1b[6nb".as_bytes().to_vec(), "ab".as_bytes().to_vec()),
        ("decxcpr", "\x1b[?6n".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("da1", "\x1b[c\x1b[0c".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("da2", "\x1b[>c\x1b[>0c".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("da3", "\x1b[=c".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("decrqm ansi", "\x1b[4$p".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("decrqm dec", "\x1b[?2004$p".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("xtversion", "\x1b[>q\x1b[>0q".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("cursor style passes", "\x1b[2 q".as_bytes().to_vec(), "\x1b[2 q".as_bytes().to_vec()),
        ("kitty query", "\x1b[?u".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("kitty push and pop pass", "\x1b[>1u\x1b[<u\x1b[=1;1u".as_bytes().to_vec(), "\x1b[>1u\x1b[<u\x1b[=1;1u".as_bytes().to_vec()),
        ("title st", "\x1b]2;my title\x1b\\".as_bytes().to_vec(), "\x1b]2;my title\x1b\\".as_bytes().to_vec()),
        ("title bel", "\x1b]0;both\x07\x1b]1;icon\x07".as_bytes().to_vec(), "\x1b]0;both\x07\x1b]1;icon\x07".as_bytes().to_vec()),
        ("title controls stripped", "\x1b]2;x\u{009b}31mred\u{009d}52;c;aGk=\x07".as_bytes().to_vec(), "\x1b]2;x31mred52;c;aGk=\x07".as_bytes().to_vec()),
        ("title c0 stripped", "\x1b]2;a\tb\x7fc\x07".as_bytes().to_vec(), "\x1b]2;abc\x07".as_bytes().to_vec()),
        ("title without text", "\x1b]2\x07".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("dcs keeps going after esc", "\x1bPq\x1bx\x1b]52;c;aGk=\x07more\x1b\\ok".as_bytes().to_vec(), "ok".as_bytes().to_vec()),
        ("cwd", "\x1b]7;file://host/home\x07".as_bytes().to_vec(), "\x1b]7;file://host/home\x07".as_bytes().to_vec()),
        ("hyperlink", "\x1b]8;;http://x\x07l\x1b]8;;\x07".as_bytes().to_vec(), "\x1b]8;;http://x\x07l\x1b]8;;\x07".as_bytes().to_vec()),
        ("prompt marks", "\x1b]133;A\x07$ \x1b]133;B\x07".as_bytes().to_vec(), "\x1b]133;A\x07$ \x1b]133;B\x07".as_bytes().to_vec()),
        ("osc 52 set", "a\x1b]52;c;aGk=\x07b".as_bytes().to_vec(), "ab".as_bytes().to_vec()),
        ("osc 52 query", "\x1b]52;c;?\x07".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("osc color query", "\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]4;1;?\x07".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("osc color set", "\x1b]4;1;rgb:ff/00/00\x07\x1b]10;#ffffff\x07".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("osc notification", "\x1b]9;hi\x07\x1b]777;notify;a;b\x07".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("osc iterm file", "\x1b]1337;File=name=eA==:aGk=\x07".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("osc number missing", "\x1b];x\x07".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("decrqss", "x\x1bP$qm\x1b\\y".as_bytes().to_vec(), "xy".as_bytes().to_vec()),
        ("xtgettcap", "\x1bP+q544e\x1b\\".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("any dcs", "\x1bP1$r0m\x1b\\".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("tmux passthrough", "a\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\b".as_bytes().to_vec(), "ab".as_bytes().to_vec()),
        ("apc kitty graphics", "a\x1b_Gf=24;AAAA\x1b\\b".as_bytes().to_vec(), "ab".as_bytes().to_vec()),
        ("pm", "\x1b^private\x1b\\".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("sos", "\x1bXstart of string\x1b\\".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("c1 csi query", "x\u{009b}6ny".as_bytes().to_vec(), "x6ny".as_bytes().to_vec()),
        ("c1 osc 52", "\u{009d}52;c;aGk=\u{009c}".as_bytes().to_vec(), "52;c;aGk=".as_bytes().to_vec()),
        ("c1 dcs", "\u{0090}tmux;\u{009c}".as_bytes().to_vec(), "tmux;".as_bytes().to_vec()),
        ("c1 nel", "a\u{0085}b".as_bytes().to_vec(), "ab".as_bytes().to_vec()),
        ("c2 that isn't c1", "\u{00a0}\u{00a9}".as_bytes().to_vec(), "\u{00a0}\u{00a9}".as_bytes().to_vec()),
        ("c1 inside csi", "\x1b[3\u{009b}1m".as_bytes().to_vec(), "\x1b[31m".as_bytes().to_vec()),
        ("c1 completed around a dropped one", b"\xc2\xc2\x9c\x90".to_vec(), "\u{fffd}\u{fffd}".as_bytes().to_vec()),
        ("broken utf-8 replaced", b"a\xffb\x80c\xe4\xb8d".to_vec(), "a\u{fffd}b\u{fffd}c\u{fffd}d".as_bytes().to_vec()),
        ("esc esc", "\x1b\x1b[6n".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("esc esc no recombination", "\x1b\x1bPq\x1b\\]52;c;aGk=\x07".as_bytes().to_vec(), "]52;c;aGk=\x07".as_bytes().to_vec()),
        ("two-byte escapes", "\x1b7\x1b8\x1bM\x1b(B".as_bytes().to_vec(), "\x1b7\x1b8\x1bM\x1b(B".as_bytes().to_vec()),
        ("csi aborted by esc", "\x1b[12\x1b[6n".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("csi aborted no recombination", "\x1b[\x1bPx\x1b\\6n".as_bytes().to_vec(), "6n".as_bytes().to_vec()),
        ("csi cancelled", "\x1b[6\x18n".as_bytes().to_vec(), "\x18n".as_bytes().to_vec()),
        ("osc aborted by esc", "\x1b]2;t\x1b[6n".as_bytes().to_vec(), "".as_bytes().to_vec()),
        ("osc aborted then text", "\x1b]2;t\x1bMx".as_bytes().to_vec(), "\x1bMx".as_bytes().to_vec()),
        ("osc cancelled", "\x1b]2;t\x18z".as_bytes().to_vec(), "\x18z".as_bytes().to_vec()),
        ("long garbage csi dropped", (String::new() + "\x1b[" + &"1;".repeat(200) + "mx").into_bytes(), "x".as_bytes().to_vec()),
        ("long csi query dropped", (String::new() + "\x1b[" + &"0;".repeat(200) + "6nx").into_bytes(), "x".as_bytes().to_vec()),
        ("long osc 52 dropped", (String::new() + "a\x1b]52;c;" + &"QUFB".repeat(3000) + "\x07b").into_bytes(), "ab".as_bytes().to_vec()),
        ("long hyperlink passes", (String::new() + "\x1b]8;;" + &"u".repeat(5000) + "\x1b\\").into_bytes(), (String::new() + "\x1b]8;;" + &"u".repeat(5000) + "\x1b\\").into_bytes()),
        ("long title dropped", (String::new() + "\x1b]2;" + &"t".repeat(5000) + "\x1b\\x").into_bytes(), "x".as_bytes().to_vec()),
        ("long dcs dropped", (String::new() + "\x1bP" + &"q".repeat(5000) + "\x1b\\x").into_bytes(), "x".as_bytes().to_vec()),
        ("long string aborted", (String::new() + "\x1b]8;;" + &"t".repeat(5000) + "\x1b[5n").into_bytes(), (String::new() + "\x1b]8;;" + &"t".repeat(5000)).into_bytes()),
        ("long osc 52 aborted", (String::new() + "\x1b]52;c;" + &"Q".repeat(5000) + "\x1bM").into_bytes(), "\x1bM".as_bytes().to_vec()),
        ("long osc 52 cancelled", (String::new() + "\x1b]52;c;" + &"Q".repeat(5000) + "\x1a").into_bytes(), "\x1a".as_bytes().to_vec()),
    ]
}

fn short(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    if s.chars().count() > 80 {
        let c: Vec<char> = s.chars().collect();
        return format!("{}…{}", c[..40].iter().collect::<String>(), c[c.len() - 40..].iter().collect::<String>());
    }
    s.into_owned()
}

#[test]
fn query_filter() {
    for (name, input, want) in filter_cases() {
        let mut f = QueryFilter::default();
        let got = f.filter(&input);
        assert!(got == want, "{name}: got {:?}, want {:?}", short(&got), short(&want));
        // Split at every byte (sampled for long cases): same output.
        let step = 1 + input.len() / 200;
        let mut cut = 0;
        while cut <= input.len() {
            let mut f = QueryFilter::default();
            let mut got = f.filter(&input[..cut]);
            got.extend(f.filter(&input[cut..]));
            assert!(got == want, "{name} cut {cut}: got {:?}", short(&got));
            cut += step;
        }
    }
}

#[test]
fn query_filter_reset() {
    let mut f = QueryFilter::default();
    assert_eq!(f.filter(b"a\x1b[6"), b"a");
    f.reset(b"");
    assert_eq!(f.filter(b"n"), b"n", "after reset");
}

/// One fuzz input, as FuzzQueryFilterSplit checks it.
fn check_split(input: &[u8], a: usize, b: usize) {
    let mut whole = QueryFilter::default();
    let want = whole.filter(input);
    let (mut i, mut j) = (a % (input.len() + 1), b % (input.len() + 1));
    if i > j {
        std::mem::swap(&mut i, &mut j);
    }
    let mut split = QueryFilter::default();
    let mut got = split.filter(&input[..i]);
    got.extend(split.filter(&input[i..j]));
    got.extend(split.filter(&input[j..]));
    assert!(got == want, "split {i},{j}: {:?}, whole {:?}", short(&got), short(&want));
    // Nothing dropped can come back by recombination: what passed passes
    // again unchanged. (Checked when the input ended between sequences and
    // characters: what is still held back is not decided yet.)
    let mut again = QueryFilter::default();
    let settled = whole.state == FState::Ground && whole.rune.is_empty();
    // A broken lead byte that passed is held again as a maybe-start.
    let mut re = again.filter(&want);
    re.extend_from_slice(&again.rune);
    assert!(!settled || re == want, "not idempotent:\n in {:?}\nout {:?}\nre  {:?}", short(input), short(&want), short(&re));
    let s = String::from_utf8_lossy(&want);
    assert!(!s.chars().any(|c| ('\u{80}'..='\u{9f}').contains(&c)), "C1 control passed: {:?}", short(&want));
}

/// Go's FuzzQueryFilterSplit over its seed corpus (each case split at a
/// half and a third) and fixed pseudo-random inputs built from the cases'
/// bytes.
#[test]
fn query_filter_split_fuzz() {
    let cases = filter_cases();
    for (_, input, _) in &cases {
        check_split(input, input.len() / 2, input.len() / 3);
    }
    let alphabet: Vec<u8> = b"\x1b[]P_^X\\\x07\x18\x1a0123456789;?$>=<cnmpqu \xc2\x9b\x9c\x9d\x90\xe4\xb8\xad\xff\x80a".to_vec();
    let mut seed = 0x9e3779b97f4a7c15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..2000 {
        let n = (next() % 48) as usize;
        let input: Vec<u8> = (0..n).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
        let (a, b) = (next() as usize, next() as usize);
        check_split(&input, a, b);
    }
}

#[test]
fn printable_lines_keeps_breaks() {
    assert_eq!(printable_lines("a\x1bb\nc\td"), "a?b\nc?d");
}
