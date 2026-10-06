use std::{ops::Deref, sync::OnceLock};
use twilight_http::{Client, client::InteractionClient};
use twilight_model::id::{Id, marker::ApplicationMarker};

pub static CTX: Ref = Ref(OnceLock::new());

#[derive(Debug)]
pub struct Context {
    pub application_id: Id<ApplicationMarker>,
    pub http: Client,
}

impl Context {
    pub fn interaction(&self) -> InteractionClient<'_> {
        self.http.interaction(self.application_id)
    }
}

pub fn init(application_id: Id<ApplicationMarker>, http: Client) {
    CTX.0
        .set(Context {
            application_id,
            http,
        })
        .expect("CTX already initialized");
}

pub struct Ref(OnceLock<Context>);

impl Deref for Ref {
    type Target = Context;

    fn deref(&self) -> &Self::Target {
        self.0.get().expect("CTX not initialized")
    }
}
