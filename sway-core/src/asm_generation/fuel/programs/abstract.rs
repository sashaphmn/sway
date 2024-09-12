use super::{AllocatedProgram, FnName, SelectorOpt};
use crate::{
    asm_generation::{
        fuel::{
            abstract_instruction_set::AbstractInstructionSet,
            allocated_abstract_instruction_set::AllocatedAbstractInstructionSet,
            compiler_constants,
            data_section::{DataSection, Entry},
            globals_section::GlobalsSection,
            register_sequencer::RegisterSequencer,
        },
        ProgramKind,
    },
    asm_lang::{
        allocated_ops::{AllocatedOpcode, AllocatedRegister},
        AllocatedAbstractOp, ConstantRegister, ControlFlowOp, Label, VirtualImmediate12,
        VirtualImmediate18, VirtualImmediate24,
    },
    decl_engine::DeclRefFunction,
    ExperimentalFlags,
};
use either::Either;
use sway_error::error::CompileError;

/// The entry point of an abstract program.
pub(crate) struct AbstractEntry {
    pub(crate) selector: SelectorOpt,
    pub(crate) label: Label,
    pub(crate) ops: AbstractInstructionSet,
    pub(crate) name: FnName,
    pub(crate) test_decl_ref: Option<DeclRefFunction>,
}

/// An [AbstractProgram] represents code generated by the compilation from IR, with virtual registers
/// and abstract control flow.
///
/// Use `AbstractProgram::to_allocated_program()` to perform register allocation.
///
pub(crate) struct AbstractProgram {
    kind: ProgramKind,
    data_section: DataSection,
    globals_section: GlobalsSection,
    before_entries: AbstractInstructionSet,
    entries: Vec<AbstractEntry>,
    non_entries: Vec<AbstractInstructionSet>,
    reg_seqr: RegisterSequencer,
    experimental: ExperimentalFlags,
}

impl AbstractProgram {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        kind: ProgramKind,
        data_section: DataSection,
        globals_section: GlobalsSection,
        before_entries: AbstractInstructionSet,
        entries: Vec<AbstractEntry>,
        non_entries: Vec<AbstractInstructionSet>,
        reg_seqr: RegisterSequencer,
        experimental: ExperimentalFlags,
    ) -> Self {
        AbstractProgram {
            kind,
            data_section,
            globals_section,
            before_entries,
            entries,
            non_entries,
            reg_seqr,
            experimental,
        }
    }

    /// True if the [AbstractProgram] does not contain any instructions, or entries, or data in the data section.
    pub(crate) fn is_empty(&self) -> bool {
        self.non_entries.is_empty()
            && self.entries.is_empty()
            && self.data_section.value_pairs.is_empty()
    }

    /// Adds prologue, globals allocation, before entries, contract method switch, and allocates virtual register
    pub(crate) fn into_allocated_program(
        mut self,
        fallback_fn: Option<crate::asm_lang::Label>,
    ) -> Result<AllocatedProgram, CompileError> {
        let mut prologue = self.build_prologue();
        self.append_globals_allocation(&mut prologue);
        self.append_before_entries(&mut prologue)?;

        match (self.experimental.new_encoding, self.kind) {
            (true, ProgramKind::Contract) => {
                self.append_jump_to_entry(&mut prologue);
            }
            (false, ProgramKind::Contract) => {
                self.append_encoding_v0_contract_abi_switch(&mut prologue, fallback_fn);
            }
            _ => {}
        }

        // Keep track of the labels (and names) that represent program entry points.
        let entries = self
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.selector,
                    entry.label,
                    entry.name.clone(),
                    entry.test_decl_ref.clone(),
                )
            })
            .collect();

        // Gather all functions
        let all_functions = self
            .entries
            .into_iter()
            .map(|entry| entry.ops)
            .chain(self.non_entries);

        // optimise and then verify these functions.
        let abstract_functions = all_functions
            .map(|instruction_set| instruction_set.optimize(&self.data_section))
            .map(AbstractInstructionSet::verify)
            .collect::<Result<Vec<AbstractInstructionSet>, CompileError>>()?;

        // Allocate the registers for each function.
        let functions = abstract_functions
            .into_iter()
            .map(|abstract_instruction_set| {
                let allocated = abstract_instruction_set.allocate_registers()?;
                Ok(allocated.emit_pusha_popa())
            })
            .collect::<Result<Vec<AllocatedAbstractInstructionSet>, CompileError>>()?;

        // XXX need to verify that the stack use for each function is balanced.

        Ok(AllocatedProgram {
            kind: self.kind,
            data_section: self.data_section,
            prologue,
            functions,
            entries,
        })
    }

    fn append_before_entries(
        &self,
        prologue: &mut AllocatedAbstractInstructionSet,
    ) -> Result<(), CompileError> {
        let before_entries = self.before_entries.clone().optimize(&self.data_section);
        let before_entries = before_entries.verify()?;
        let mut before_entries = before_entries.allocate_registers()?;
        prologue.ops.append(&mut before_entries.ops);
        Ok(())
    }

    /// Builds the asm preamble, which includes metadata and a jump past the metadata.
    /// Right now, it looks like this:
    ///
    /// WORD OP
    /// 1    MOV $scratch $pc
    /// -    JMPF $zero i10
    /// 2    DATA_START (0-32) (in bytes, offset from $is)
    /// -    DATA_START (32-64)
    /// 3    METADATA (0-32)
    /// -    METADATA (32-64)
    /// 4    METADATA (64-96)
    /// -    METADATA (96-128)
    /// 5    METADATA (128-160)
    /// -    METADATA (160-192)
    /// 6    METADATA (192-224)
    /// -    METADATA (224-256)
    /// 7    LW $ds $scratch 1
    /// -    ADD $ds $ds $scratch
    /// 8    .program_start:
    fn build_prologue(&mut self) -> AllocatedAbstractInstructionSet {
        const _: () = assert!(
            crate::PRELUDE_METADATA_OFFSET_IN_BYTES == 16,
            "Inconsistency in the assumption of prelude organisation"
        );
        const _: () = assert!(
            crate::PRELUDE_METADATA_SIZE_IN_BYTES == 32,
            "Inconsistency in the assumption of prelude organisation"
        );
        const _: () = assert!(
            crate::PRELUDE_SIZE_IN_BYTES == 56,
            "Inconsistency in the assumption of prelude organisation"
        );
        let label = self.reg_seqr.get_label();
        AllocatedAbstractInstructionSet {
            ops: [
                AllocatedAbstractOp {
                    opcode: Either::Left(AllocatedOpcode::MOVE(
                        AllocatedRegister::Constant(ConstantRegister::Scratch),
                        AllocatedRegister::Constant(ConstantRegister::ProgramCounter),
                    )),
                    comment: String::new(),
                    owning_span: None,
                },
                // word 1.5
                AllocatedAbstractOp {
                    opcode: Either::Right(ControlFlowOp::Jump(label)),
                    comment: String::new(),
                    owning_span: None,
                },
                // word 2 -- full word u64 placeholder
                AllocatedAbstractOp {
                    opcode: Either::Right(ControlFlowOp::DataSectionOffsetPlaceholder),
                    comment: "data section offset".into(),
                    owning_span: None,
                },
                // word 3 -- 32 bytes placeholder
                AllocatedAbstractOp {
                    opcode: Either::Right(ControlFlowOp::Metadata),
                    comment: "metadata".into(),
                    owning_span: None,
                },
                AllocatedAbstractOp {
                    opcode: Either::Right(ControlFlowOp::Label(label)),
                    comment: "end of metadata".into(),
                    owning_span: None,
                },
                // word 7 -- load the data offset into $ds
                AllocatedAbstractOp {
                    opcode: Either::Left(AllocatedOpcode::LW(
                        AllocatedRegister::Constant(ConstantRegister::DataSectionStart),
                        AllocatedRegister::Constant(ConstantRegister::Scratch),
                        VirtualImmediate12::new_unchecked(1, "1 doesn't fit in 12 bits"),
                    )),
                    comment: "".into(),
                    owning_span: None,
                },
                // word 7.5 -- add $ds $ds $is
                AllocatedAbstractOp {
                    opcode: Either::Left(AllocatedOpcode::ADD(
                        AllocatedRegister::Constant(ConstantRegister::DataSectionStart),
                        AllocatedRegister::Constant(ConstantRegister::DataSectionStart),
                        AllocatedRegister::Constant(ConstantRegister::Scratch),
                    )),
                    comment: "".into(),
                    owning_span: None,
                },
            ]
            .to_vec(),
        }
    }

    // WHen the new encoding is used, jumps to the `__entry`  function
    fn append_jump_to_entry(&mut self, asm: &mut AllocatedAbstractInstructionSet) {
        let entry = self.entries.iter().find(|x| x.name == "__entry").unwrap();
        asm.ops.push(AllocatedAbstractOp {
            opcode: Either::Right(ControlFlowOp::Jump(entry.label)),
            comment: "jump to ABI function selector".into(),
            owning_span: None,
        });
    }

    /// Builds the contract switch statement based on the first argument to a contract call: the
    /// 'selector'.
    /// See https://fuellabs.github.io/fuel-specs/master/vm#call-frames which
    /// describes the first argument to be at word offset 73.
    fn append_encoding_v0_contract_abi_switch(
        &mut self,
        asm: &mut AllocatedAbstractInstructionSet,
        fallback_fn: Option<crate::asm_lang::Label>,
    ) {
        const SELECTOR_WORD_OFFSET: u64 = 73;
        const INPUT_SELECTOR_REG: AllocatedRegister = AllocatedRegister::Allocated(0);
        const PROG_SELECTOR_REG: AllocatedRegister = AllocatedRegister::Allocated(1);
        const CMP_RESULT_REG: AllocatedRegister = AllocatedRegister::Allocated(2);

        // Build the switch statement for selectors.
        asm.ops.push(AllocatedAbstractOp {
            opcode: Either::Right(ControlFlowOp::Comment),
            comment: "[function selection]: begin contract function selector switch".into(),
            owning_span: None,
        });

        // Load the selector from the call frame.
        asm.ops.push(AllocatedAbstractOp {
            opcode: Either::Left(AllocatedOpcode::LW(
                INPUT_SELECTOR_REG,
                AllocatedRegister::Constant(ConstantRegister::FramePointer),
                VirtualImmediate12::new_unchecked(
                    SELECTOR_WORD_OFFSET,
                    "constant infallible value",
                ),
            )),
            comment: "[function selection]: load input function selector".into(),
            owning_span: None,
        });

        // Add a 'case' for each entry with a selector.
        for entry in &self.entries {
            let selector = match entry.selector {
                Some(sel) => sel,
                // Skip entries that don't have a selector - they're probably tests.
                None => continue,
            };

            // Put the selector in the data section.
            let data_label = self.data_section.insert_data_value(Entry::new_word(
                u32::from_be_bytes(selector) as u64,
                None,
                None,
            ));

            // Load the data into a register for comparison.
            asm.ops.push(AllocatedAbstractOp {
                opcode: Either::Left(AllocatedOpcode::LoadDataId(PROG_SELECTOR_REG, data_label)),
                comment: format!(
                    "[function selection]: load function {} selector for comparison",
                    entry.name
                ),
                owning_span: None,
            });

            // Compare with the input selector.
            asm.ops.push(AllocatedAbstractOp {
                opcode: Either::Left(AllocatedOpcode::EQ(
                    CMP_RESULT_REG,
                    INPUT_SELECTOR_REG,
                    PROG_SELECTOR_REG,
                )),
                comment: format!(
                    "[function selection]: compare function {} selector with input selector",
                    entry.name
                ),
                owning_span: None,
            });

            // Jump to the function label if the selector was equal.
            asm.ops.push(AllocatedAbstractOp {
                // If the comparison result is _not_ equal to 0, then it was indeed equal.
                opcode: Either::Right(ControlFlowOp::JumpIfNotZero(CMP_RESULT_REG, entry.label)),
                comment: "[function selection]: jump to selected contract function".into(),
                owning_span: None,
            });
        }

        if let Some(fallback_fn) = fallback_fn {
            asm.ops.push(AllocatedAbstractOp {
                opcode: Either::Right(ControlFlowOp::Call(fallback_fn)),
                comment: "[function selection]: call contract fallback function".into(),
                owning_span: None,
            });
        }

        asm.ops.push(AllocatedAbstractOp {
            opcode: Either::Left(AllocatedOpcode::MOVI(
                AllocatedRegister::Constant(ConstantRegister::Scratch),
                VirtualImmediate18 {
                    value: compiler_constants::MISMATCHED_SELECTOR_REVERT_CODE,
                },
            )),
            comment: "[function selection]: load revert code for mismatched function selector"
                .into(),
            owning_span: None,
        });
        asm.ops.push(AllocatedAbstractOp {
            opcode: Either::Left(AllocatedOpcode::RVRT(AllocatedRegister::Constant(
                ConstantRegister::Scratch,
            ))),
            comment: "[function selection]: revert if no selectors have matched".into(),
            owning_span: None,
        });
    }

    fn append_globals_allocation(&self, asm: &mut AllocatedAbstractInstructionSet) {
        let len_in_bytes = self.globals_section.len_in_bytes();
        asm.ops.push(AllocatedAbstractOp {
            opcode: Either::Left(AllocatedOpcode::CFEI(VirtualImmediate24 {
                value: len_in_bytes as u32,
            })),
            comment: "allocate stack space for globals".into(),
            owning_span: None,
        });
    }
}

impl std::fmt::Display for AbstractProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, ";; Program kind: {:?}", self.kind)?;

        writeln!(f, ";; --- Before Entries ---")?;
        writeln!(f, "{}\n", self.before_entries)?;

        writeln!(f, ";; --- Entries ---")?;
        for entry in &self.entries {
            writeln!(f, "{}\n", entry.ops)?;
        }
        writeln!(f, ";; --- Functions ---")?;
        for function in &self.non_entries {
            writeln!(f, "{function}\n")?;
        }
        writeln!(f, ";; --- Data ---")?;
        write!(f, "{}", self.data_section)
    }
}
