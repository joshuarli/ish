use epsh::encoding::{bytes_to_str, str_to_bytes};
use epsh::eval::Shell;
use epsh::parser::Parser;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::process::Command;
use std::sync::{Arc, Mutex};

#[test]
fn codec_preserves_every_private_use_scalar_and_invalid_byte() {
    for scalar in 0xe000..=0xf8ff {
        let text = char::from_u32(scalar).unwrap().to_string();
        let mut bytes = vec![0x80, b'a'];
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(&[0xff, 0xc0, 0x80]);
        assert_eq!(str_to_bytes(&bytes_to_str(&bytes)), bytes, "{scalar:x}");
    }
    for byte in 0..=255u8 {
        let bytes = [byte, 0xee, 0x82, 0x80, byte];
        assert_eq!(str_to_bytes(&bytes_to_str(&bytes)), bytes);
    }
}

fn captured(source: &[u8], text_entry: bool) -> Vec<u8> {
    let output = Arc::new(Mutex::new(Vec::new()));
    let mut shell = Shell::new();
    shell.set_stdout_sink(output.clone());
    let status = if text_entry {
        shell.run_script(std::str::from_utf8(source).unwrap())
    } else {
        shell.run_program(&Parser::new_bytes(source).parse().unwrap()).code()
    };
    assert_eq!(status, 0);
    let result = output.lock().unwrap().clone();
    result
}

#[test]
fn parser_text_and_byte_sources_preserve_literal_private_use_characters() {
    let value = "\u{e000}\u{e080}\u{e0ff}";
    for source in [
        format!("printf '%s' '{value}'"),
        format!("printf '%s' \"${{UNSET_CODEC:-{value}}}\""),
        format!("printf '%s' \"$(printf '%s' '{value}')\""),
    ] {
        assert_eq!(captured(source.as_bytes(), true), value.as_bytes());
        assert_eq!(captured(source.as_bytes(), false), value.as_bytes());
    }
    let source = b"printf '%s' '\xee\x82\x80\x80\xee\x80\x80\xff'";
    assert_eq!(captured(source, false), b"\xee\x82\x80\x80\xee\x80\x80\xff");
}

#[test]
fn public_text_values_are_utf8_and_encoded_values_decode_explicitly() {
    let text = "\u{e000}\u{e080}\u{e0ff}";
    assert_eq!(epsh::shell_bytes::ShellBytes::from(text).as_bytes(), text.as_bytes());
    assert_eq!(epsh::shell_bytes::ShellBytes::from(text.to_string()).as_bytes(), text.as_bytes());
    let mut shell = Shell::new();
    shell.set_var("CODEC", text).unwrap();
    shell.set_args(&["codec", text]);
    assert_eq!(shell.get_var_bytes("CODEC").unwrap().as_bytes(), text.as_bytes());
    let output = Arc::new(Mutex::new(Vec::new()));
    shell.set_stdout_sink(output.clone());
    assert_eq!(shell.run_script("printf '%s%s' \"$CODEC\" \"$1\""), 0);
    assert_eq!(*output.lock().unwrap(), text.as_bytes().repeat(2));
}

#[test]
fn lexer_spans_count_source_characters_without_codec_prefixes() {
    let mut lexer = epsh::lexer::Lexer::new("\u{e000}\u{e080}\u{e0ff} next");
    lexer.next_token().unwrap();
    let (_, span) = lexer.next_token().unwrap();
    assert_eq!(span.offset, 4);
    assert_eq!(span.col, 5);
    assert_eq!(span.line, 1);
}

#[test]
fn private_use_units_survive_ifs_patterns_and_parameter_trimming() {
    let pua = "\u{e000}\u{e080}\u{e0ff}";
    let script = format!(
        "v='{pua}'; printf '%s:%s:%s:' \"${{v#?}}\" \"${{v%?}}\" \"${{#v}}\"; IFS='\u{e080}'; v='a\u{e080}b'; printf '<%s>' $v"
    );
    let expected = "\u{e080}\u{e0ff}:\u{e000}\u{e080}:9:<a><b>";
    assert_eq!(captured(script.as_bytes(), true), expected.as_bytes());
    for scalar in ["\u{e000}", "\u{e080}", "\u{e0ff}"] {
        assert!(epsh::glob::fnmatch("?", scalar));
        assert!(!epsh::glob::fnmatch("??", scalar));
        assert!(epsh::glob::fnmatch(&format!("[{scalar}]"), scalar));
    }
    let script = b"v='\xee\x82\x80\x80'; printf '%s:%s' \"${v#?}\" \"${v%?}\"";
    assert_eq!(captured(script, false), b"\x80:\xee\x82\x80");
}

#[test]
fn quoted_private_use_heredocs_and_nested_scripts_preserve_bytes() {
    let value = "\u{e000}\u{e080}\u{e0ff}";
    for script in [
        format!("printf '%s' \"${{UNSET_CODEC:-\\{value}}}\""),
        format!("eval \"printf '%s' '{value}'\""),
        format!("trap \"printf '%s' '{value}'\" EXIT"),
    ] {
        let expected = if script.starts_with("printf") { format!("\\{value}") } else { value.to_string() };
        assert_eq!(captured(script.as_bytes(), true), expected.as_bytes(), "{script}");
    }
    for quoted in [false, true] {
        let delimiter = if quoted { "'EOF'" } else { "EOF" };
        let script = format!("cat <<{delimiter}\n\\{value}\nEOF\n");
        let output = Command::new(env!("CARGO_BIN_EXE_epsh")).arg("-c").arg(script).output().unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(output.stdout, format!("\\{value}\n").as_bytes());
    }
}

#[test]
fn cli_source_arguments_and_environment_preserve_private_use_and_invalid_bytes() {
    let value = [0xee, 0x80, 0x80, 0xee, 0x82, 0x80, 0xff];
    let name = "CODEC_\u{e080}";
    let mut source = b"printf '%s' '".to_vec();
    source.extend_from_slice(&value);
    source.extend_from_slice(b"'; printf '%s' \"$1\"; printf '%s' \"$CODEC_VALUE\"");
    let output = Command::new(env!("CARGO_BIN_EXE_epsh"))
        .arg("-c")
        .arg(OsString::from_vec(source))
        .arg("codec")
        .arg(OsString::from_vec(value.to_vec()))
        .env("CODEC_VALUE", OsString::from_vec(value.to_vec()))
        .env(name, OsString::from_vec(value.to_vec()))
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, value.repeat(3));
    let output = Command::new(env!("CARGO_BIN_EXE_epsh"))
        .arg("-c")
        .arg(format!("'{}' '{name}'", env!("CARGO_BIN_EXE_show_env")))
        .env(name, OsString::from_vec(value.to_vec()))
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"ee8080ee8280ff\n");
}
