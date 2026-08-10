# Linting Issues

This document records the current linting issues found by `ruff` in the codebase. While the code is functional and tests pass, these issues represent potential improvements for code quality and maintainability.

## Summary

- **Total Issues**: 100
- **Status**: Tests passing ✅
- **Functionality**: All features working correctly

## Issue Breakdown by Category

### Print Statements (T201) - 31 issues
**Rationale**: These are intentional console output for CLI functionality
- Various print statements for user feedback, debugging, and process status
- Located throughout `src/running_process/running_process.py`

### Blind Exception Handling (BLE001) - 17 issues
**Rationale**: Many are appropriate for process management and cleanup scenarios
- Exception handling in process termination, cleanup, and error recovery
- Located in process management methods

### Missing stacklevel in warnings (B028) - 10 issues
**Minor improvement**: Add `stacklevel=2` to warnings.warn() calls
- Located in various error handling blocks
- Easy fix: Add `stacklevel=2` parameter

### Commented-out code (ERA001) - 5 issues
**Note**: May be intentional for documentation/reference
- Commented code blocks in process cleanup sections
- Consider removing or converting to proper documentation

### Import location (PLC0415) - 4 issues
**Context**: Some imports are within functions to avoid circular dependencies
- Imports within exception handlers and method bodies
- May be necessary for the current module structure

### Complex functions (C901) - 3 issues
**Major refactoring needed**:
1. `ProcessOutputReader.run()` (complexity 12 > 10)
2. `RunningProcess.get_next_line()` (complexity 16 > 10)
3. `RunningProcess.wait()` (complexity 20 > 10)

### Exception string literals (EM101/EM102) - 3 issues
**Minor**: Extract exception messages to variables
- Located in exception raising statements
- Easy fix: Assign message to variable first

### Try-except-pass patterns (S110/SIM105) - 2 issues
**Improvement**: Use `contextlib.suppress()` or add logging
- Located in cleanup and error recovery code

### Other Issues - 25 issues
- **UP035**: Deprecated `typing.ContextManager` usage (1)
- **N818**: Exception naming convention (1)
- **E402**: Module level import not at top (1)
- **F841**: Unused variable (1)
- **TRY300**: Missing else blocks (4)
- **PLR1714**: Comparison optimization (1)
- **PTH108**: Use Path.unlink() instead of os.unlink() (1)
- **S602/S603**: Subprocess security warnings (2)
- **SIM103**: Boolean logic simplification (1)
- **SLF001**: Private member access (1)
- **B904**: Exception chaining (1)
- **ANN204**: Missing return type annotation (1)

## Detailed Issues

```
warning: The following rules have been removed and ignoring them has no effect:
    - ANN101
    - ANN102


UP035 `typing.ContextManager` is deprecated, use `contextlib.AbstractContextManager` instead
  --> src\running_process\running_process.py:15:1

N818 Exception name `EndOfStream` should be named with an Error suffix
  --> src\running_process\running_process.py:18:7

E402 Module level import not at top of file
  --> src\running_process\running_process.py:22:1

C901 `run` is too complex (12 > 10)
  --> src\running_process\running_process.py:58:9

BLE001 Do not catch blind exception: `Exception`
  --> src\running_process\running_process.py:64:20

B028 No explicit `stacklevel` keyword argument found
  --> src\running_process\running_process.py:65:17

T201 `print` found
  --> src\running_process\running_process.py:87:17

T201 `print` found
  --> src\running_process\running_process.py:88:17

SIM105 Use `contextlib.suppress(Exception)` instead of `try`-`except`-`pass`
  --> src\running_process\running_process.py:91:17

S110 `try`-`except`-`pass` detected, consider logging the exception
  --> src\running_process\running_process.py:93:17

BLE001 Do not catch blind exception: `Exception`
  --> src\running_process\running_process.py:93:24

[... and 89 more similar issues throughout the codebase]
```

## Recommendations

### High Priority
1. **Fix complex functions** - Break down the 3 functions with high complexity
2. **Add stacklevel to warnings** - Simple 1-line fixes for 10 issues
3. **Fix import structure** - Address deprecated imports and module organization

### Medium Priority
1. **Exception message extraction** - Convert string literals to variables
2. **Use contextlib.suppress()** - Replace try-except-pass patterns
3. **Remove unused variables** - Clean up unused assignments

### Low Priority
1. **Print statement annotations** - Add `# noqa: T201` if intentional
2. **Commented code cleanup** - Remove or document commented sections
3. **Security annotations** - Add `# noqa: S602/S603` for subprocess calls if safe

### Consider Keeping
- **Print statements**: Essential for CLI user feedback
- **Broad exception handling**: Appropriate for process cleanup/recovery
- **Some imports in functions**: May be necessary to avoid circular dependencies

## Import Resolution Guidelines

**Use full relative imports for module resolution**:
- Use `from .module import Class` instead of `from module import Class`
- This ensures proper package structure and avoids ModuleNotFoundError
- Example: `from .output_formatter import OutputFormatter` not `from output_formatter import OutputFormatter`

## Notes

- The codebase is **fully functional** with all tests passing
- Many issues are **acceptable for CLI tools** (console output, process management)
- Focus should be on **complex function refactoring** as the highest impact improvement
- Print statements and some exception handling may be **intentional design choices**