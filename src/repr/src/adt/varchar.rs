// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::error::Error;

use anyhow::bail;
use serde::{Deserialize, Serialize};

use mz_lowertest::MzReflect;
use mz_ore::cast::CastFrom;

use std::fmt;

// The `Arbitrary` impls are only used during testing and we gate them
// behind `cfg(test)`, so `proptest` can remain a dev-dependency.
// See https://altsysrq.github.io/proptest-book/proptest-derive/getting-started.html
// for guidance on using `derive(Arbitrary)` outside of test code.
#[cfg(test)]
use proptest_derive::Arbitrary;

// https://github.com/postgres/postgres/blob/REL_14_0/src/include/access/htup_details.h#L577-L584
pub const MAX_MAX_LENGTH: u32 = 10_485_760;

/// A marker type indicating that a Rust string should be interpreted as a
/// [`ScalarType::VarChar`].
///
/// [`ScalarType::VarChar`]: crate::ScalarType::VarChar
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct VarChar<S: AsRef<str>>(pub S);

/// The `max_length` of a [`ScalarType::VarChar`].
///
/// This newtype wrapper ensures that the length is within the valid range.
///
/// [`ScalarType::VarChar`]: crate::ScalarType::VarChar
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize, MzReflect,
)]
#[cfg_attr(test, derive(Arbitrary))]
pub struct VarCharMaxLength(pub(crate) u32);

impl VarCharMaxLength {
    /// Consumes the newtype wrapper, returning the inner `u32`.
    pub fn into_u32(self) -> u32 {
        self.0
    }
}

impl TryFrom<i64> for VarCharMaxLength {
    type Error = InvalidVarCharMaxLengthError;

    fn try_from(max_length: i64) -> Result<Self, Self::Error> {
        match u32::try_from(max_length) {
            Ok(max_length) if max_length > 0 && max_length < MAX_MAX_LENGTH => {
                Ok(VarCharMaxLength(max_length))
            }
            _ => Err(InvalidVarCharMaxLengthError),
        }
    }
}

/// The error returned when constructing a [`VarCharMaxLength`] from an invalid
/// value.
#[derive(Debug, Clone)]
pub struct InvalidVarCharMaxLengthError;

impl fmt::Display for InvalidVarCharMaxLengthError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "length for type character varying must be between 1 and {}",
            MAX_MAX_LENGTH
        )
    }
}

impl Error for InvalidVarCharMaxLengthError {}

pub fn format_str(
    s: &str,
    length: Option<VarCharMaxLength>,
    fail_on_len: bool,
) -> Result<String, anyhow::Error> {
    Ok(match length {
        // Note that length is 1-indexed, so finding `None` means the string's
        // characters don't exceed the length, while finding `Some` means it
        // does.
        Some(l) => {
            let l = usize::cast_from(l.into_u32());
            match s.char_indices().nth(l) {
                None => s.to_string(),
                Some((idx, _)) => {
                    if !fail_on_len || s[idx..].chars().all(|c| c.is_ascii_whitespace()) {
                        s[..idx].to_string()
                    } else {
                        bail!("{} exceeds maximum length of {}", s, l)
                    }
                }
            }
        }
        None => s.to_string(),
    })
}
