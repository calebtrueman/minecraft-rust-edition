//! Indexed field references shared by the synchronous field instructions.

use crate::JavaError::{NullPointerException, UnsupportedOperationException};
use crate::Result;
use crate::field_ref_cache::{ResolvedFieldRef, resolve_field_ref};
use crate::frame::{ExecutionResult, Frame};
use crate::operand_stack::OperandStack;
use ristretto_classfile::attributes::Instruction;
use std::sync::OnceLock;

impl ResolvedFieldRef {
    fn check_host_policy(
        &self,
        frame: &Frame,
        stack: &OperandStack,
        instruction: &Instruction,
    ) -> Result<()> {
        let thread = frame.thread()?;
        let vm = thread.vm()?;
        let Some(policy) = vm.configuration().method_execution_policy() else {
            return Ok(());
        };

        let is_read = matches!(
            instruction,
            Instruction::Getfield(_) | Instruction::Getstatic(_)
        );
        let receiver = match instruction {
            Instruction::Getfield(_) => Some(stack.peek_at(0)?),
            Instruction::Putfield(_) => Some(stack.peek_at(1)?),
            _ => None,
        };
        let receiver_class_name = match receiver {
            Some(crate::Value::Object(Some(reference))) => Some(reference.read().class_name()?),
            _ => None,
        };
        let owner_class_name = self.referenced_class.name();
        let declaring_class_name = self.declaring_class.name();
        let Some(blocked_class_name) = policy
            .blocked_field_class(owner_class_name, receiver_class_name.as_deref())
            .or_else(|| {
                policy.blocked_field_class(declaring_class_name, receiver_class_name.as_deref())
            })
        else {
            return Ok(());
        };

        let field_signature = self.signature();
        let field_allowed = is_read
            && receiver.is_some_and(|receiver| {
                policy.field_read_allowed_on_host_object(receiver, &field_signature)
            });
        if field_allowed {
            return Ok(());
        }

        Err(UnsupportedOperationException(format!(
            "Java field instruction for {} is blocked by the host policy for class {blocked_class_name}",
            self.signature()
        ))
        .into())
    }

    fn execute(
        &self,
        frame: &Frame,
        stack: &mut OperandStack,
        instruction: &Instruction,
    ) -> Result<ExecutionResult> {
        if matches!(
            instruction,
            Instruction::Putfield(_) | Instruction::Putstatic(_)
        ) {
            self.check_write(frame)?;
        }
        match instruction {
            Instruction::Getstatic(_) => stack.push(self.get_static()?)?,
            Instruction::Putstatic(_) => self.put_static(stack.pop()?)?,
            Instruction::Getfield(_) => {
                let object = stack.pop()?;
                if object.is_null() {
                    return Err(NullPointerException(None).into());
                }
                let object = object.as_object_ref()?;
                stack.push(self.get(&object)?)?;
            }
            Instruction::Putfield(_) => {
                let value = stack.pop()?;
                let object = stack.pop()?;
                if object.is_null() {
                    return Err(NullPointerException(None).into());
                }
                let mut object = object.as_object_mut()?;
                self.put(&mut object, value)?;
            }
            _ => {
                return Err(crate::Error::InternalError(format!(
                    "Expected field instruction: {instruction:?}"
                )));
            }
        }
        Ok(ExecutionResult::Continue)
    }
}

pub(crate) fn try_field(
    frame: &Frame,
    stack: &mut OperandStack,
    index: u16,
    instruction: &Instruction,
) -> Result<Option<ExecutionResult>> {
    let Some(resolved) = frame.field_refs()?.get(index).and_then(OnceLock::get) else {
        return Ok(None);
    };
    resolved.check_kind(matches!(
        instruction,
        Instruction::Getstatic(_) | Instruction::Putstatic(_)
    ))?;
    resolved.check_host_policy(frame, stack, instruction)?;
    if resolved.is_static() && !resolved.declaring_class.is_initialized()? {
        return Ok(None);
    }
    resolved.execute(frame, stack, instruction).map(Some)
}

pub(crate) async fn execute_field(
    frame: &Frame,
    stack: &mut OperandStack,
    index: u16,
    instruction: Instruction,
) -> Result<ExecutionResult> {
    let resolved = resolve_field_ref(frame, index).await?;
    resolved.check_kind(matches!(
        instruction,
        Instruction::Getstatic(_) | Instruction::Putstatic(_)
    ))?;
    resolved.check_host_policy(frame, stack, &instruction)?;
    if matches!(
        instruction,
        Instruction::Putfield(_) | Instruction::Putstatic(_)
    ) {
        resolved.check_write(frame)?;
    }
    if resolved.is_static() && !resolved.declaring_class.is_initialized()? {
        frame
            .thread()?
            .initialize_class(&resolved.declaring_class)
            .await?;
    }
    resolved.execute(frame, stack, &instruction)
}
