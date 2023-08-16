// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

fn main() -> std::io::Result<()> {
    prost_build::Config::new()
        .bytes(["."])
        .compile_protos(&["proto/dev/restate/storage/v1/domain.proto"], &["proto"])
}
