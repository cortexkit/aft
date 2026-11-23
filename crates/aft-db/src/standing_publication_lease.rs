/// A lease whose current ownership must be checked before standing publication.
#[doc(hidden)]
pub trait StandingPublicationLease {
    fn verify(&self) -> std::io::Result<bool>;
}

impl<T: StandingPublicationLease + ?Sized> StandingPublicationLease for std::sync::Arc<T> {
    fn verify(&self) -> std::io::Result<bool> {
        self.as_ref().verify()
    }
}
