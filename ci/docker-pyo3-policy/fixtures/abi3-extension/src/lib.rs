use pyo3::prelude::*;

#[pyfunction]
fn answer() -> u32 {
    42
}

#[pymodule]
fn soldr_pyo3_policy_fixture(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(answer, module)?)?;
    Ok(())
}
