//! #497 / slice 33 of #500: an impl that smuggles state into `bind`
//! by adding a `&self` parameter must fail to compile.
//!
//! `BrokeredBackend::bind` is a static method (no receiver) so an impl
//! adding `&self` is a signature mismatch. rustc rejects it; trybuild
//! verifies the diagnostic stays stable.

use running_process::broker::brokered_backend::{
    BindError, BrokeredBackend, Endpoint, IpcListener, Never,
};

struct WrongBackend {
    state: u32,
}

impl BrokeredBackend for WrongBackend {
    type State = u32;

    // ERROR: bind must be a static method per the trait — no `&self`.
    fn bind(&self, _endpoint: &Endpoint) -> Result<IpcListener, BindError> {
        let _ = self.state;
        unimplemented!()
    }

    fn serve(_listener: IpcListener) -> Never {
        unimplemented!()
    }
}

fn main() {}
