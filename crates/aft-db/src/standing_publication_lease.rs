/// A lease whose current ownership must be checked before standing publication.
#[doc(hidden)]
pub trait StandingPublicationLease {
    fn verify(&self) -> std::io::Result<bool>;
}
