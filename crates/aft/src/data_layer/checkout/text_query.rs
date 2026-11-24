//! Finding a view's text candidates.

use aft_store::text::{candidates as scoped_candidates, candidates_among, Query, Scope};

use super::{CheckoutView, DataError};

/// [`CheckoutView::text_candidates`]. A whole-checkout search reads the
/// view's grouped members, built once on the view's first lookup, rather
/// than walking, grouping and sorting every published path for each pattern.
/// A directory search still walks only that directory's paths.
pub(super) fn candidates(
    view: &CheckoutView,
    query: &Query,
    scope: Scope<'_>,
) -> Result<Vec<Vec<u8>>, DataError> {
    let published = &view.published;
    let texts = published.texts()?;
    match scope {
        Scope::All => candidates_among(
            &published.state,
            view.members(&texts),
            &texts,
            &view.text,
            query,
        ),
        Scope::Directory(_) => scoped_candidates(
            &published.state,
            &published.membership,
            &texts,
            &view.text,
            query,
            scope,
        ),
    }
    .map_err(DataError::store)
}
