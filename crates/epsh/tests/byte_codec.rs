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
