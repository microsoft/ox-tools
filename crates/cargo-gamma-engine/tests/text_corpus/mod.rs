// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub(super) const CONTROLS: &[&str] = &[
    "",
    "é🦀中",
    "é\0\t\n\r\u{7f}\u{80}\u{9f}🦀",
    "\u{1b}é\u{1b}\u{80}\u{1b}🦀",
    "\u{9b}31m\u{9d}8;;https://example.test\u{9c}",
    "\u{a0}é\u{7f}中\u{a0}",
];

pub(super) const SEQUENCES: &[&str] = &[
    "\x1b[mé\x1b[0m",
    "\x1b[1;22;31;49m中",
    "\x1b[38;5;255m🦀\x1b[48;2;0;128;255m",
    "\x1b[38:2::0:128:255mé\x1b[48:5:0m",
    "\x1b[00031mé",
    "\x1b[8mhidden\x1b[28m",
    "\x1b[38;5;256m\x1b[38:2:0:1m",
    "\x1b[31K\x1b[31J\x1b[31A\x1b[?25l",
    "\x1b]8;;https://example.test\x07é\x1b]8;;\x1b\\",
    "\x1b[\x1b[31;\x1b[38:2::1:2:\x1b",
    "\x1b[31mé\x1b[0m\n\u{9b}2K",
];
