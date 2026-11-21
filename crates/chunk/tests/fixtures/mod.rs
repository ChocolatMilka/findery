//! One realistic file per language, shared by the integration tests.
//!
//! INTENT: the golden tests (`languages.rs`) and the coverage invariant
//! (`coverage.rs`) must read the SAME sources — a fixture the invariant never
//! sees is a fixture whose dropped lines nobody would notice.

#![allow(dead_code)]

pub const JS_SRC: &str = "\
import { readFileSync } from 'node:fs';

const DEFAULTS = { retries: 3 };

// Retry with exponential backoff.
async function withBackoff(fn, attempts = DEFAULTS.retries) {
  for (let i = 0; i < attempts; i++) {
    try {
      return await fn();
    } catch (e) {
      if (i === attempts - 1) throw e;
    }
  }
}

export class Client {
  constructor(base) {
    this.base = base;
  }

  async get(path) {
    return fetch(`${this.base}${path}`);
  }
}

export default Client;
";

pub const TS_SRC: &str = "\
export interface RetryOptions {
  attempts: number;
}

export type Handler = (code: number) => string;

export const MAX_ATTEMPTS = 5;

export function retry<T>(fn: () => T, options: RetryOptions): T {
  return fn();
}
";

pub const PYTHON_SRC: &str = "\
\"\"\"Rate limiting.\"\"\"

import time

DEFAULT_WINDOW = 60


def sleep_backoff(attempt):
    \"\"\"Sleep for the attempt's window.\"\"\"
    time.sleep(2 ** attempt)


class Limiter:
    \"\"\"A token bucket.\"\"\"

    def __init__(self, limit):
        self.limit = limit

    def take(self):
        return self.limit > 0
";

pub const PHP_SRC: &str = "\
<?php

declare(strict_types=1);

namespace App\\Http;

use App\\Support\\Signer;

class Client
{
    public const TIMEOUT = 30;

    public function __construct(private Signer $signer)
    {
    }

    public function send(string $body): string
    {
        return $this->signer->sign($body);
    }
}
";

pub const JSON_SRC: &str = "\
{
  \"name\": \"demo\",
  \"dependencies\": {
    \"left-pad\": \"1.3.0\",
    \"lodash\": \"4.17.21\"
  },
  \"scripts\": { \"build\": \"tsc\", \"test\": \"jest\" },
  \"packageManager\": \"pnpm\"
}
";

pub const RUST_SRC: &str = "\
//! Backoff.

use std::time::Duration;

/// How many times to retry.
pub const MAX_ATTEMPTS: u32 = 5;

pub fn with_backoff(attempt: u32) -> Duration {
    Duration::from_millis(1 << attempt)
}

/// The retry policy.
#[derive(Debug)]
pub struct Policy {
    pub attempts: u32,
}

/// Retries under the policy.
impl Policy {
    pub fn next(&self, attempt: u32) -> Option<Duration> {
        (attempt < self.attempts).then(|| with_backoff(attempt))
    }
}
";

pub const JAVA_SRC: &str = "\
package app.http;

import java.io.IOException;
import java.util.List;

/** Talks to the API. */
@Service
public class Client {
    private static final int MAX_RETRIES = 3;

    private final String base;

    public Client(String base) {
        this.base = base;
    }

    /** Sends with backoff. */
    @Override
    public <T> List<T> send(String body) throws IOException {
        String q = \"\"\"
            { \"retries\": 3 }
            \"\"\";
        char open = '{';
        return List.of();
    }
}

@interface Audited {}
";

pub const KOTLIN_SRC: &str = "\
package app.http

import kotlin.math.max
import kotlinx.coroutines.*

const val MAX_RETRIES = 3

/** Talks to the API. */
class Client(private val base: String) : Api {
    override suspend fun send(body: String): String {
        val q = \"\"\"{ \"url\": \"${base}/x\" }\"\"\"
        return \"${body.length} }\"
    }

    companion object {
        fun create() = Client(\"x\")
    }
}

fun String.slug(): String = lowercase()

@Test
fun `sends once`() {
}
";

pub const GO_SRC: &str = "\
// Package client talks to the API.
package client

import (
\t\"fmt\"
\t\"testing\"
)

// MaxRetries bounds the backoff.
const MaxRetries = 3

type Client struct {
\tbase string `json:\"base\"`
}

// Send sends one body.
func (c *Client) Send(body string) error {
\tq := `{ \"retries\": 3 }`
\tr := '{'
\treturn fmt.Errorf(\"%s %c %s\", q, r, body)
}

func TestSend(t *testing.T) {
}
";

pub const AXLE_SRC: &str = "\
use std::io::Path;

/** Sort direction. */
enum Sort {
    Up,
    Down,
}

@link(symbol = \"getpid\")
extern \"C\" fn libc_getpid() : i32;

pub(crate) class Buf<T> {
    data : T[];

    constructor(n : i64) {
        self.data = new T[n];
    }

    /** The capacity. */
    pub fn cap(self) : i64 {
        return 0;
    }

    drop() {}
}
";

pub const MARKDOWN_SRC: &str = "\
---
title: Guide
---

Some preamble before the first heading.

# Guide

intro text here

## Install

```sh
# not a heading
cargo add it
```

    # an indented comment

Windows
-------

use msvc
";

/// Every fixture above, with the path that routes it to its chunker.
pub const ALL: &[(&str, &str)] = &[
    ("src/client.js", JS_SRC),
    ("src/retry.ts", TS_SRC),
    ("app/limits.py", PYTHON_SRC),
    ("src/Http/Client.php", PHP_SRC),
    ("package.json", JSON_SRC),
    ("src/backoff.rs", RUST_SRC),
    ("src/main/java/app/http/Client.java", JAVA_SRC),
    ("src/main/kotlin/app/http/Client.kt", KOTLIN_SRC),
    ("client/client_test.go", GO_SRC),
    ("std/buf.axle", AXLE_SRC),
    ("docs/guide.md", MARKDOWN_SRC),
];
