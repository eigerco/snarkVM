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

impl<A: Aleo> Visibility<A> for Plaintext<A> {
    /// Returns the number of field elements to encode `self`.
    fn size_in_fields(&self) -> u16 {
        // Compute the number of bits.
        let num_bits = self.to_bits_le().len() + 1; // 1 extra bit for the terminus indicator.
        // Compute the ceiling division of the number of bits by the number of bits in a field element.
        let num_fields = (num_bits + A::BaseField::size_in_data_bits() - 1) / A::BaseField::size_in_data_bits();
        // Ensure the number of field elements does not exceed the maximum allowed size.
        match num_fields <= A::MAX_DATA_SIZE_IN_FIELDS as usize {
            // Return the number of field elements.
            true => num_fields as u16,
            false => A::halt("Plaintext is too large to encode in field elements."),
        }
    }
}
