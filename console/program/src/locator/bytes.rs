// Copyright 2024 Aleo Network Foundation
// This file is part of the snarkVM library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:

// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::*;

impl<N: Network> FromBytes for Locator<N> {
    /// Reads the locator from a buffer.
    fn read_le<R: Read>(mut reader: R) -> IoResult<Self> {
        let id = FromBytes::read_le(&mut reader)?;
        let resource = FromBytes::read_le(&mut reader)?;
        Ok(Self { id, resource })
    }
}

impl<N: Network> ToBytes for Locator<N> {
    /// Writes the locator to a buffer.
    fn write_le<W: Write>(&self, mut writer: W) -> IoResult<()> {
        self.id.write_le(&mut writer)?;
        self.resource.write_le(&mut writer)
    }
}
