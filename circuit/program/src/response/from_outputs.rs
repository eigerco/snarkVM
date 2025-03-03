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

impl<A: Aleo> Response<A> {
    /// Initializes a response, given the number of inputs, tvk, tcm, outputs, output types, and output registers.
    pub fn from_outputs(
        network_id: &U16<A>,
        program_id: &ProgramID<A>,
        function_name: &Identifier<A>,
        num_inputs: usize,
        tvk: &Field<A>,
        tcm: &Field<A>,
        outputs: Vec<Value<A>>,
        output_types: &[console::ValueType<A::Network>], // Note: Console type
        output_registers: &[Option<console::Register<A::Network>>], // Note: Console type
    ) -> Self {
        // Compute the function ID.
        let function_id = compute_function_id(network_id, program_id, function_name);

        // Compute the output IDs.
        let output_ids = outputs
            .iter()
            .zip_eq(output_types)
            .zip_eq(output_registers)
            .enumerate()
            .map(|(index, ((output, output_type), output_register))| {
                match output_type {
                    // For a constant output, compute the hash (using `tcm`) of the output.
                    console::ValueType::Constant(..) => {
                        // Prepare the index as a constant field element.
                        let output_index = Field::constant(console::Field::from_u16((num_inputs + index) as u16));
                        // Construct the preimage as `(function ID || output || tcm || index)`.
                        let mut preimage = Vec::new();
                        preimage.push(function_id.clone());
                        preimage.extend(output.to_fields());
                        preimage.push(tcm.clone());
                        preimage.push(output_index);

                        // Hash the output to a field element.
                        match &output {
                            // Return the output ID.
                            Value::Plaintext(..) => OutputID::constant(A::hash_psd8(&preimage)),
                            // Ensure the output is a plaintext.
                            Value::Record(..) => A::halt("Expected a plaintext output, found a record output"),
                            Value::Future(..) => A::halt("Expected a plaintext output, found a future output"),
                        }
                    }
                    // For a public output, compute the hash (using `tcm`) of the output.
                    console::ValueType::Public(..) => {
                        // Prepare the index as a constant field element.
                        let output_index = Field::constant(console::Field::from_u16((num_inputs + index) as u16));
                        // Construct the preimage as `(function ID || output || tcm || index)`.
                        let mut preimage = Vec::new();
                        preimage.push(function_id.clone());
                        preimage.extend(output.to_fields());
                        preimage.push(tcm.clone());
                        preimage.push(output_index);

                        // Hash the output to a field element.
                        match &output {
                            // Return the output ID.
                            Value::Plaintext(..) => OutputID::public(A::hash_psd8(&preimage)),
                            // Ensure the output is a plaintext.
                            Value::Record(..) => A::halt("Expected a plaintext output, found a record output"),
                            Value::Future(..) => A::halt("Expected a plaintext output, found a future output"),
                        }
                    }
                    // For a private output, compute the ciphertext (using `tvk`) and hash the ciphertext.
                    console::ValueType::Private(..) => {
                        // Prepare the index as a constant field element.
                        let output_index = Field::constant(console::Field::from_u16((num_inputs + index) as u16));
                        // Compute the output view key as `Hash(function ID || tvk || index)`.
                        let output_view_key = A::hash_psd4(&[function_id.clone(), tvk.clone(), output_index]);
                        // Compute the ciphertext.
                        let ciphertext = match &output {
                            Value::Plaintext(plaintext) => plaintext.encrypt_symmetric(output_view_key),
                            // Ensure the output is a plaintext.
                            Value::Record(..) => A::halt("Expected a plaintext output, found a record output"),
                            Value::Future(..) => A::halt("Expected a plaintext output, found a future output"),
                        };
                        // Return the output ID.
                        OutputID::private(A::hash_psd8(&ciphertext.to_fields()))
                    }
                    // For a record output, compute the record commitment, and encrypt the record (using `tvk`).
                    console::ValueType::Record(record_name) => {
                        // Retrieve the record.
                        let record = match &output {
                            Value::Record(record) => record,
                            // Ensure the output is a record.
                            Value::Plaintext(..) => A::halt("Expected a record output, found a plaintext output"),
                            Value::Future(..) => A::halt("Expected a record output, found a future output"),
                        };

                        // Retrieve the output register.
                        let output_register = match output_register {
                            Some(output_register) => output_register,
                            None => A::halt("Expected a register to be paired with a record output"),
                        };

                        // Compute the record commitment.
                        let commitment = record.to_commitment(program_id, &Identifier::constant(*record_name));

                        // Prepare the index as a constant field element.
                        let output_index = Field::constant(console::Field::from_u64(output_register.locator()));
                        // Compute the encryption randomizer as `HashToScalar(tvk || index)`.
                        let randomizer = A::hash_to_scalar_psd2(&[tvk.clone(), output_index]);

                        // Encrypt the record, using the randomizer.
                        let encrypted_record = record.encrypt(&randomizer);
                        // Compute the record checksum, as the hash of the encrypted record.
                        let checksum = A::hash_bhp1024(&encrypted_record.to_bits_le());

                        // Return the output ID.
                        OutputID::record(commitment, checksum)
                    }
                    // For an external record output, compute the hash (using `tvk`) of the output.
                    console::ValueType::ExternalRecord(..) => {
                        // Prepare the index as a constant field element.
                        let output_index = Field::constant(console::Field::from_u16((num_inputs + index) as u16));
                        // Construct the preimage as `(function ID || output || tvk || index)`.
                        let mut preimage = Vec::new();
                        preimage.push(function_id.clone());
                        preimage.extend(output.to_fields());
                        preimage.push(tvk.clone());
                        preimage.push(output_index);

                        // Return the output ID.
                        match &output {
                            Value::Record(..) => OutputID::external_record(A::hash_psd8(&preimage)),
                            // Ensure the output is a record.
                            Value::Plaintext(..) => A::halt("Expected a record output, found a plaintext output"),
                            Value::Future(..) => A::halt("Expected a record output, found a future output"),
                        }
                    }
                    // For a future output, compute the hash (using `tcm`) of the output.
                    console::ValueType::Future(..) => {
                        // Prepare the index as a constant field element.
                        let output_index = Field::constant(console::Field::from_u16((num_inputs + index) as u16));
                        // Construct the preimage as `(function ID || output || tcm || index)`.
                        let mut preimage = Vec::new();
                        preimage.push(function_id.clone());
                        preimage.extend(output.to_fields());
                        preimage.push(tcm.clone());
                        preimage.push(output_index);

                        // Hash the output to a field element.
                        match &output {
                            // Return the output ID.
                            Value::Future(..) => OutputID::future(A::hash_psd8(&preimage)),
                            // Ensure the output is a future.
                            Value::Plaintext(..) => A::halt("Expected a future output, found a plaintext output"),
                            Value::Record(..) => A::halt("Expected a future output, found a record output"),
                        }
                    }
                }
            })
            .collect();

        // Return the response.
        Self { output_ids, outputs }
    }
}

#[cfg(all(test, feature = "console"))]
mod tests {
    use super::*;
    use crate::Circuit;
    use snarkvm_utilities::{TestRng, Uniform};

    use anyhow::Result;
    use snarkvm_circuit_types::U16;

    pub(crate) const ITERATIONS: usize = 20;

    fn check_from_outputs(
        mode: Mode,
        num_constants: u64,
        num_public: u64,
        num_private: u64,
        num_constraints: u64,
    ) -> Result<()> {
        use console::Network;

        let rng = &mut TestRng::default();

        for i in 0..ITERATIONS {
            // Sample a `tvk`.
            let tvk = console::Field::rand(rng);
            // Compute the transition commitment as `Hash(tvk)`.
            let tcm = <Circuit as Environment>::Network::hash_psd2(&[tvk])?;

            // Compute the nonce.
            let index = console::Field::from_u64(8);
            let randomizer = <Circuit as Environment>::Network::hash_to_scalar_psd2(&[tvk, index]).unwrap();
            let nonce = <Circuit as Environment>::Network::g_scalar_multiply(&randomizer);

            // Construct the outputs.
            let output_constant = console::Value::<<Circuit as Environment>::Network>::Plaintext(
                console::Plaintext::from_str("{ token_amount: 9876543210u128 }").unwrap(),
            );
            let output_public = console::Value::<<Circuit as Environment>::Network>::Plaintext(
                console::Plaintext::from_str("{ token_amount: 9876543210u128 }").unwrap(),
            );
            let output_private = console::Value::<<Circuit as Environment>::Network>::Plaintext(
                console::Plaintext::from_str("{ token_amount: 9876543210u128 }").unwrap(),
            );
            let output_record = console::Value::<<Circuit as Environment>::Network>::Record(console::Record::from_str(&format!("{{ owner: aleo1d5hg2z3ma00382pngntdp68e74zv54jdxy249qhaujhks9c72yrs33ddah.private, token_amount: 100u64.private, _nonce: {nonce}.public }}")).unwrap());
            let output_external_record = console::Value::<<Circuit as Environment>::Network>::Record(console::Record::from_str("{ owner: aleo1d5hg2z3ma00382pngntdp68e74zv54jdxy249qhaujhks9c72yrs33ddah.private, token_amount: 100u64.private, _nonce: 0group.public }").unwrap());
            let outputs = vec![output_constant, output_public, output_private, output_record, output_external_record];

            // Construct the output types.
            let output_types = vec![
                console::ValueType::from_str("amount.constant").unwrap(),
                console::ValueType::from_str("amount.public").unwrap(),
                console::ValueType::from_str("amount.private").unwrap(),
                console::ValueType::from_str("token.record").unwrap(),
                console::ValueType::from_str("token.aleo/token.record").unwrap(),
            ];

            // Construct the output registers.
            let output_registers = vec![
                Some(console::Register::Locator(5)),
                Some(console::Register::Locator(6)),
                Some(console::Register::Locator(7)),
                Some(console::Register::Locator(8)),
                Some(console::Register::Locator(9)),
            ];

            // Construct a network ID.
            let network_id = console::U16::new(<Circuit as Environment>::Network::ID);
            // Construct a program ID.
            let program_id = console::ProgramID::from_str("test.aleo")?;
            // Construct a function name.
            let function_name = console::Identifier::from_str("check")?;

            // Construct the response.
            let response = console::Response::new(
                &network_id,
                &program_id,
                &function_name,
                4,
                &tvk,
                &tcm,
                outputs.clone(),
                &output_types,
                &output_registers,
            )?;

            // Inject the network ID, program ID, function name, `tvk`, `tcm`, and outputs.
            let network_id = U16::<Circuit>::constant(network_id);
            let program_id = ProgramID::<Circuit>::new(mode, program_id);
            let function_name = Identifier::<Circuit>::new(mode, function_name);
            let tvk = Field::<Circuit>::new(mode, tvk);
            let tcm = Field::<Circuit>::new(mode, tcm);
            let outputs = Inject::new(mode, outputs);

            Circuit::scope(format!("Response {i}"), || {
                // Compute the response using outputs (circuit).
                let candidate = Response::from_outputs(
                    &network_id,
                    &program_id,
                    &function_name,
                    4,
                    &tvk,
                    &tcm,
                    outputs,
                    &output_types,
                    &output_registers,
                );
                assert_eq!(response, candidate.eject_value());
                match mode.is_constant() {
                    true => assert_scope!(<=num_constants, <=num_public, <=num_private, <=num_constraints),
                    false => assert_scope!(<=num_constants, num_public, num_private, num_constraints),
                }
            });
            Circuit::reset();
        }
        Ok(())
    }

    // Note: These counts are correct. At this (high) level of a program, we override the default mode in many cases,
    // based on the user-defined visibility in the types. Thus, we have nonzero public, private, and constraint values.

    #[test]
    fn test_from_outputs_constant() -> Result<()> {
        check_from_outputs(Mode::Constant, 26000, 6, 9500, 9500)
    }

    #[test]
    fn test_from_outputs_public() -> Result<()> {
        check_from_outputs(Mode::Public, 24849, 6, 13962, 13983)
    }

    #[test]
    fn test_from_outputs_private() -> Result<()> {
        check_from_outputs(Mode::Private, 24849, 6, 13962, 13983)
    }
}
