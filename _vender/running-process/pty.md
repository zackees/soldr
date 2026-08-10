# PTY Integration Investigation for RunningProcess

## What is PTY and Why Use It?

A PTY (pseudo-terminal) simulates a terminal interface, allowing processes to behave as if they're running in an interactive terminal rather than a pipe. This is essential for:

- **Interactive commands** that behave differently when connected to a terminal vs pipe
- **Programs requiring terminal control** (e.g., password prompts, progress bars, colored output)
- **Shell features** that only work with a controlling terminal (e.g., `ssh -t`, interactive shells)
- **Avoiding buffering issues** where programs buffer output differently for pipes vs terminals

## Current RunningProcess Architecture Analysis

The current `RunningProcess` class uses standard subprocess pipes:

```python
# From running_process.py:349-362
self.proc = subprocess.Popen(
    popen_command,
    shell=self.shell,
    cwd=self.cwd,
    stdout=subprocess.PIPE,      # Standard pipe
    stderr=subprocess.STDOUT,    # Merged to stdout pipe
    text=True,
    encoding="utf-8",
    errors="replace",
)
```

The `ProcessOutputReader` (src/running_process/process_output_reader.py:67-79) reads from `proc.stdout` in a dedicated thread:

```python
for line in self._proc.stdout:
    # Process each line from the pipe
```

## PTY Integration Approaches

### Approach 1: Optional PTY Mode (Recommended)

Add an optional `use_pty` parameter to `RunningProcess.__init__()`:

```python
def __init__(
    self,
    command: str | list[str],
    use_pty: bool = False,  # New parameter
    # ... existing parameters
):
```

**Implementation Strategy:**
- Modify `_create_process()` to use `pty.openpty()` when `use_pty=True`
- Replace `subprocess.PIPE` with PTY file descriptors
- Update `ProcessOutputReader` to read from PTY master instead of stdout pipe
- Handle PTY-specific cleanup and error conditions

### Approach 2: Separate PTY Class

Create a `RunningProcessPTY` subclass that inherits from `RunningProcess` and overrides PTY-specific methods.

## Technical Implementation Details

### PTY Process Creation
```python
import pty
import os

def _create_process_with_pty(self) -> None:
    """Create subprocess with PTY allocation."""
    master_fd, slave_fd = pty.openpty()

    self.proc = subprocess.Popen(
        popen_command,
        shell=self.shell,
        cwd=self.cwd,
        stdin=slave_fd,
        stdout=slave_fd,
        stderr=slave_fd,  # All streams use PTY
        text=True,
        encoding="utf-8",
        errors="replace",
        preexec_fn=os.setsid,  # Create new session
    )

    os.close(slave_fd)  # Parent doesn't need slave
    self.master_fd = master_fd
```

### ProcessOutputReader Modifications

The `ProcessOutputReader` would need updates to read from PTY master:

```python
def _process_stdout_lines(self) -> None:
    """Process PTY output lines."""
    if hasattr(self._proc, 'master_fd'):
        # PTY mode: read from master file descriptor
        import select
        while not self._shutdown.is_set():
            ready, _, _ = select.select([self._proc.master_fd], [], [], 0.1)
            if ready:
                try:
                    data = os.read(self._proc.master_fd, 4096)
                    if not data:
                        break
                    lines = data.decode('utf-8', errors='replace').splitlines()
                    for line in lines:
                        # Process line as before
                except (OSError, ValueError):
                    break
    else:
        # Standard pipe mode (existing code)
        for line in self._proc.stdout:
            # ... existing implementation
```

## Cross-Platform Compatibility Issues

### Unix/Linux/macOS
- Full PTY support via `pty` module
- Works with `os.read()`, `select.select()`
- Standard POSIX terminal control

### Windows
- **Major limitation**: Python's `pty` module is Unix-only
- Windows alternatives:
  - `pywinpty` library for Windows 10+
  - `pexpect` with `winpexpect` backend
  - Native Windows pseudo-console APIs (Windows 10 1903+)

### Recommended Cross-Platform Strategy
```python
import sys

if sys.platform == 'win32':
    # Use winpty or disable PTY mode
    try:
        import winpty
        # Implement Windows PTY support
    except ImportError:
        raise NotImplementedError("PTY mode requires winpty on Windows")
else:
    # Use standard Unix pty module
    import pty
```

## Integration Challenges

### 1. Output Reading Complexity
- PTY requires `os.read()` instead of file iteration
- Need `select()` for non-blocking reads
- Handle partial reads and line buffering manually

### 2. Process Lifecycle
- PTY processes may need different termination handling
- Session management with `os.setsid()`
- Proper cleanup of PTY file descriptors

### 3. Error Handling
- PTY-specific errors (EIO when process exits)
- File descriptor cleanup on exceptions
- Cross-platform error message handling

### 4. Thread Safety
- File descriptor operations need careful synchronization
- PTY master should only be read from one thread

## Recommendations

### Phase 1: Basic PTY Support (Unix-only)
1. Add `use_pty` parameter to `RunningProcess`
2. Implement PTY creation in `_create_process()`
3. Update `ProcessOutputReader` for PTY file descriptor reading
4. Add comprehensive error handling and cleanup

### Phase 2: Cross-Platform Support
1. Add Windows support via `winpty` or similar
2. Implement platform detection and fallback logic
3. Create integration tests for both platforms

### Phase 3: Advanced Features
1. PTY size control and terminal settings
2. Support for interactive input (stdin to PTY)
3. Advanced terminal emulation features

## Code Impact Assessment

**Low Impact Areas:**
- External API (if `use_pty` defaults to `False`)
- Most existing functionality remains unchanged
- Backward compatibility maintained

**Medium Impact Areas:**
- `_create_process()` method needs conditional logic
- `ProcessOutputReader.run()` needs PTY branch
- Additional cleanup in `kill()` method

**High Impact Areas:**
- Cross-platform testing requirements
- Documentation updates
- New dependency management (winpty for Windows)

## Testing Requirements

1. **Unit Tests**: PTY creation, cleanup, error handling
2. **Integration Tests**: Interactive commands (bash, ssh, password prompts)
3. **Cross-Platform Tests**: Unix and Windows compatibility
4. **Performance Tests**: PTY vs pipe performance comparison
5. **Edge Cases**: Process crashes, PTY EOF, large output

## Investigation Results (September 2025)

### Current Platform Analysis (Windows 10 MSYS/Git Bash)

**Platform Detection:**
- `sys.platform`: `win32`
- `os.name`: `nt`
- Windows Version: 10.0.19045.6216

**PTY Library Availability:**
- ✅ **winpty**: Available and functional
- ❌ **Python pty module**: Not available (requires `termios` which is Unix-only)
- ❌ **pexpect**: Not available in current environment
- ✅ **Windows ConPTY**: Available (Windows 10 1903+ pseudo-console support)

### Functional Testing Results

**winpty Integration Test:**
```python
# Successfully tested winpty.PtyProcess.spawn()
pty_proc = winpty.PtyProcess.spawn(['cmd', '/c', 'echo Hello PTY World'])
# PTY process creation: ✅ WORKS
# Output reading: ✅ WORKS (with ANSI escape sequences)
# Process control: ✅ WORKS (kill, terminate, close)
```

**Key Differences: PTY vs Subprocess Pipes:**
- **Subprocess pipe**: Clean line-by-line output: `"Standard pipe"`, `"Done"`
- **PTY output**: Raw terminal output with ANSI codes: `"\x1b[1t\x1b[c\x1b[?1004h\x1b[?9001hPTY output \r\nDone\r\n"`
- **ANSI filtering required**: PTY output needs escape sequence removal for clean text

### Architecture Integration Analysis

**Current ProcessOutputReader (process_output_reader.py:65-79):**
```python
def _process_stdout_lines(self) -> None:
    assert self._proc.stdout is not None
    for line in self._proc.stdout:  # File object iteration
        # Process line...
```

**Required PTY Integration Points:**
1. **Process Creation** (`running_process.py:349-362`): Replace `subprocess.PIPE` with PTY
2. **Output Reading** (`process_output_reader.py:65-79`): Replace `self._proc.stdout` iteration with PTY read methods
3. **Cleanup** (`process_output_reader.py:109-116`): Handle PTY-specific cleanup vs stdout.close()

### Updated Implementation Strategy

**Recommended Approach: Conditional PTY Backend**
```python
class RunningProcess:
    def __init__(self, command, use_pty=False, **kwargs):
        self.use_pty = use_pty and self._pty_available()
        # ... existing init

    def _pty_available(self) -> bool:
        """Check PTY support for current platform."""
        if sys.platform == 'win32':
            try:
                import winpty
                return True
            except ImportError:
                return False
        else:
            try:
                import pty
                return True
            except ImportError:
                return False
```

**ProcessOutputReader PTY Adaptation:**
```python
def _process_stdout_lines(self) -> None:
    if hasattr(self._proc, '_pty_proc'):
        # PTY mode: use winpty/pty read methods
        self._process_pty_output()
    else:
        # Standard pipe mode (existing implementation)
        assert self._proc.stdout is not None
        for line in self._proc.stdout:
            # ... existing code

def _process_pty_output(self) -> None:
    """Process PTY output with ANSI filtering."""
    import re
    ansi_escape = re.compile(r'\x1b\[[^a-zA-Z]*[a-zA-Z]')

    while not self._shutdown.is_set():
        try:
            if sys.platform == 'win32':
                chunk = self._proc._pty_proc.read()
            else:
                chunk = os.read(self._proc._pty_fd, 4096).decode('utf-8', errors='replace')

            if not chunk:
                break

            # Filter ANSI escape sequences
            clean_chunk = ansi_escape.sub('', chunk)
            lines = clean_chunk.replace('\r\n', '\n').replace('\r', '\n').split('\n')

            for line in lines:
                if line.strip():
                    self.last_stdout_ts = time.time()
                    transformed_line = self._output_formatter.transform(line.strip())
                    self._on_output(transformed_line)

        except Exception as e:
            # Handle PTY-specific errors (EIO, etc.)
            break
```

### Cross-Platform Implementation Matrix

| Platform | PTY Library | Status | Implementation Notes |
|----------|-------------|--------|---------------------|
| Windows 10+ | winpty | ✅ Available | Use `winpty.PtyProcess.spawn()` |
| Windows 10+ | ConPTY | ✅ Available | Use `ctypes` with Windows APIs |
| Unix/Linux | pty | ✅ Available | Use `pty.openpty()` with `subprocess.Popen` |
| macOS | pty | ✅ Available | Use `pty.openpty()` with `subprocess.Popen` |

### Updated Challenges and Solutions

**1. ANSI Escape Sequence Handling**
- **Challenge**: PTY output includes terminal control sequences
- **Solution**: Implement regex-based ANSI filtering in ProcessOutputReader
- **Code**: `re.compile(r'\x1b\[[^a-zA-Z]*[a-zA-Z]').sub('', output)`

**2. Line Ending Normalization**
- **Challenge**: PTY uses `\r\n` (Windows) vs `\n` (Unix) inconsistently
- **Solution**: Normalize to `\n` in PTY output processor
- **Code**: `chunk.replace('\r\n', '\n').replace('\r', '\n')`

**3. Non-blocking Read Requirements**
- **Challenge**: PTY reads may block unlike file iteration
- **Solution**: Implement timeout-based reading with small chunks
- **Platform-specific**: winpty has built-in non-blocking, Unix needs `select()`

**4. Process Tree Management**
- **Challenge**: PTY processes may have different termination behavior
- **Solution**: Extend `kill_process_tree()` to handle PTY process cleanup
- **Implementation**: Close PTY before killing process tree

### Performance Considerations

**Memory Usage:**
- PTY requires buffering chunks vs line-by-line processing
- Recommendation: Use 4KB read chunks to balance memory and latency

**CPU Usage:**
- ANSI filtering adds regex processing overhead
- Recommendation: Compile regex once, reuse for all filtering

**Latency:**
- PTY may have different buffering characteristics
- Testing shows similar latency to pipes for this use case

## Conclusion

PTY integration is **feasible and recommended** for RunningProcess. Investigation confirms:

✅ **Cross-platform support available**: winpty (Windows) + pty (Unix/macOS)
✅ **Minimal architecture changes required**: Conditional backend in ProcessOutputReader
✅ **Backward compatibility maintained**: Optional `use_pty=False` default
✅ **Performance acceptable**: Similar to existing pipe implementation

**Key Implementation Requirements:**
1. Add `use_pty` parameter with platform detection
2. Extend ProcessOutputReader with PTY output processing
3. Implement ANSI escape sequence filtering
4. Add PTY-specific cleanup and error handling
5. Update process creation logic for PTY allocation

**Next Steps:**
1. Implement basic PTY support for Windows (winpty) and Unix (pty)
2. Add comprehensive test suite covering PTY vs pipe behavior
3. Document PTY-specific features and limitations
4. Consider adding advanced PTY features (terminal size, interactive input)

