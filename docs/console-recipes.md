<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# Console Recipes

Worked examples for common serial targets. Every target differs, so treat the
values here as starting points and check them against what your console shows.

## `exec` or `write` + `read`

Use `serial.exec` for a command and its response. It holds one lock across the
write and the read, so the response cannot be attributed to another request.

Use `serial.write` for raw input that has no response to wait for, such as a
keystroke at a menu. It returns only `bytes_written`; follow it with
`serial.read` or `serial.read_until` to see what the device did.

A single payload, including its line ending, is at most 4096 bytes. Larger
writes are refused.

## Set a line ending

The default `line_ending` is `none`, which sends exactly the text you pass.
Without a terminator, a shell never runs the command, and the next one is
appended to it: `help` then `uname -a` becomes `helpuname -a`. Set `line_ending`
at `serial.open`, in a device profile, or on a single `serial.exec` call.

## Wait for the exact prompt

`expect` is a regex that ends the read at its first match. A broad pattern such
as `[#>$]` can match a `#` inside command output and stop early. Match the whole
prompt instead, and escape regex characters such as `$`.

Command echo counts as output. If the prompt text can appear in the echoed
command, set `echo_mode` to `line` on a console that echoes one line, so matching
starts after the echo.

## Linux shell

```json
{"port": "/dev/ttyUSB0", "line_ending": "lf"}
```

```json
{
  "session_id": "<from serial.open>",
  "command": "uname -a",
  "expect": "root@myboard:~# ",
  "timeout_ms": 5000,
  "clear_before_write": true,
  "normalize_output": true
}
```

`raw_output` keeps the echo, prompts, CRLF, and ANSI sequences. `normalized_output`
is a cleaned copy.

## U-Boot

U-Boot often expects a carriage return, and its default prompt is `=> `.

```json
{"port": "/dev/ttyUSB0", "line_ending": "cr"}
```

```json
{
  "session_id": "<from serial.open>",
  "command": "version",
  "expect": "=> ",
  "timeout_ms": 5000,
  "clear_before_write": true
}
```

Commands here can write flash or memory directly. Check what you are about to
send, and run destructive commands only with explicit authorization.

## Zephyr shell

The usual prompt is `uart:~$ `, so escape the `$` in `expect`.

```json
{"port": "/dev/ttyACM0", "line_ending": "cr"}
```

```json
{
  "session_id": "<from serial.open>",
  "command": "kernel version",
  "expect": "uart:~\\$ ",
  "echo_mode": "line",
  "timeout_ms": 5000,
  "clear_before_write": true
}
```

- There is no login, so the shell is effectively privileged from the first byte.
- Log lines can interleave with command output. Read `raw_output` with that in
  mind, and do not assume a line belongs to your command.
- There are no OSC 3008 markers, so leave `semantic_prompt` unset. To see the
  previous command's return value, run `retval`.
- `echo_mode: line` keeps the echoed command out of matching. `command_output`
  still includes the trailing prompt.
- The shell wraps its echo at 80 columns with a line break, so the echo of a long
  command will not match the text you sent.
- The prompt is colored and the read stops at the match, so part of its color
  code can stay in the buffer. Set `clear_before_write` on the next call.
- `normalized_output` removes cursor-movement codes, so indented text such as
  `help` output loses its indentation. Use `raw_output` when layout matters.
- A console on the chip's own USB (CDC-ACM) disappears when the board resets. The
  session can then fail or reach EOF, and the port may return under a different
  number. Check `serial.list_ports` and open it again. A console through an
  external USB-UART adapter or debug probe stays connected across a reset.

## Raw byte exchange

For a monitor or a protocol prompt with no shell, keep `line_ending` as `none`
and send exactly what the target expects with `serial.write`, then use
`serial.read_until`. Data is lossy UTF-8 text in both directions, so this is not
a binary transport.
