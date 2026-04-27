/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use common::{Server, auth::AccessToken};
use email::{
    cache::{MessageCacheFetch, email::MessageCacheAccess},
    message::metadata::{
        ArchivedMetadataPartType, DecodedPartContent, MessageMetadata, MetadataHeaderName,
    },
};
use jmap_proto::{
    method::{
        query::Filter,
        search_snippet::{GetSearchSnippetRequest, GetSearchSnippetResponse, SearchSnippet},
    },
    object::email::EmailFilter,
    request::MaybeInvalid,
};
use futures_util::{StreamExt, TryStreamExt, stream};
use mail_parser::decoders::html::html_to_text;
use nlp::language::{Language, search_snippet::generate_snippet, stemmer::Stemmer};
use std::future::Future;
use store::{
    ValueKey,
    backend::MAX_TOKEN_LENGTH,
    write::{AlignedBytes, Archive},
};
use trc::AddContext;
use types::{acl::Acl, collection::Collection, field::EmailField, id::Id};
use utils::chained_bytes::ChainedBytes;

enum SnippetOutcome {
    Found(SearchSnippet),
    NotFound(MaybeInvalid<Id>),
}

pub trait EmailSearchSnippet: Sync + Send {
    fn email_search_snippet(
        &self,
        request: GetSearchSnippetRequest,
        access_token: &AccessToken,
    ) -> impl Future<Output = trc::Result<GetSearchSnippetResponse>> + Send;
}

impl EmailSearchSnippet for Server {
    async fn email_search_snippet(
        &self,
        request: GetSearchSnippetRequest,
        access_token: &AccessToken,
    ) -> trc::Result<GetSearchSnippetResponse> {
        let mut filter_stack = vec![];
        let mut include_term = true;
        let mut terms = vec![];
        let mut is_exact = false;
        let mut language = self.core.email.default_language;

        for cond in request.filter {
            match cond {
                Filter::Property(cond) => {
                    if let EmailFilter::Text(text)
                    | EmailFilter::Subject(text)
                    | EmailFilter::Body(text) = cond
                        && include_term
                    {
                        let (text, language_) =
                            Language::detect(text, self.core.email.default_language);
                        language = language_;
                        if (text.starts_with('"') && text.ends_with('"'))
                            || (text.starts_with('\'') && text.ends_with('\''))
                        {
                            for token in language.tokenize_text(&text, MAX_TOKEN_LENGTH) {
                                terms.push(token.word.into_owned());
                            }
                            is_exact = true;
                        } else {
                            for token in Stemmer::new(&text, language, MAX_TOKEN_LENGTH) {
                                terms.push(token.word.into_owned());
                                if let Some(stemmed_word) = token.stemmed_word {
                                    terms.push(stemmed_word.into_owned());
                                }
                            }
                        }
                    }
                }
                Filter::And | Filter::Or => {
                    filter_stack.push(cond);
                }
                Filter::Not => {
                    filter_stack.push(cond);
                    include_term = !include_term;
                }
                Filter::Close => {
                    if matches!(filter_stack.pop(), Some(Filter::Not)) {
                        include_term = !include_term;
                    }
                }
            }
        }
        let account_id = request.account_id.document_id();
        let cached_messages = self
            .get_cached_messages(account_id)
            .await
            .caused_by(trc::location!())?;
        let document_ids = if access_token.is_member(account_id) {
            cached_messages.email_document_ids()
        } else {
            cached_messages.shared_messages(access_token, Acl::ReadItems)
        };

        let email_ids = request.email_ids.unwrap();
        let mut response = GetSearchSnippetResponse {
            account_id: request.account_id,
            list: Vec::with_capacity(email_ids.len()),
            not_found: None,
        };
        let mut not_found = Vec::new();

        if email_ids.len() > self.core.jmap.snippet_max_results {
            return Err(trc::JmapEvent::RequestTooLarge.into_err());
        }

        // Fetch blobs concurrently: each iteration of this loop awaits a
        // single blob_store get_blob, and serialising them stalls the response
        // when the blob backend has non-trivial per-request latency.
        let concurrency = self.core.jmap.snippet_concurrency.max(1);
        let terms = &terms;
        let document_ids = &document_ids;

        let outcomes: Vec<SnippetOutcome> = stream::iter(email_ids)
            .map(|email_id| async move {
                // Preserve unparsable ids in `notFound` instead of dropping them.
                let email_id = match email_id {
                    MaybeInvalid::Value(email_id) => email_id,
                    invalid => return Ok::<_, trc::Error>(SnippetOutcome::NotFound(invalid)),
                };
                let document_id = email_id.document_id();
                if !document_ids.contains(document_id) {
                    return Ok(SnippetOutcome::NotFound(MaybeInvalid::Value(email_id)));
                }

                let mut snippet = SearchSnippet {
                    email_id,
                    subject: None,
                    preview: None,
                };

                if terms.is_empty() {
                    return Ok(SnippetOutcome::Found(snippet));
                }

                let metadata_ = match self
                    .store()
                    .get_value::<Archive<AlignedBytes>>(ValueKey::property(
                        account_id,
                        Collection::Email,
                        document_id,
                        EmailField::Metadata,
                    ))
                    .await?
                {
                    Some(metadata) => metadata,
                    None => return Ok(SnippetOutcome::NotFound(MaybeInvalid::Value(email_id))),
                };
                let metadata = metadata_
                    .unarchive::<MessageMetadata>()
                    .caused_by(trc::location!())?;

                // Add subject snippet
                let contents = &metadata.contents[0];
                if let Some(subject) = contents
                    .root_part()
                    .header_value(&MetadataHeaderName::Subject)
                    .and_then(|v| v.as_text())
                    .and_then(|v| generate_snippet(v, terms, language, is_exact))
                {
                    snippet.subject = subject.into();
                }

                // Download message
                let raw_body = if let Some(raw_body) = self
                    .blob_store()
                    .get_blob(metadata.blob_hash.0.as_slice(), 0..usize::MAX)
                    .await?
                {
                    raw_body
                } else {
                    trc::event!(
                        Store(trc::StoreEvent::NotFound),
                        AccountId = account_id,
                        DocumentId = email_id.document_id(),
                        Collection = Collection::Email,
                        BlobId = metadata.blob_hash.0.as_slice(),
                        Details = "Blob not found.",
                        CausedBy = trc::location!(),
                    );
                    return Ok(SnippetOutcome::NotFound(MaybeInvalid::Value(email_id)));
                };
                let raw_message = ChainedBytes::new(metadata.raw_headers.as_ref()).with_last(
                    raw_body
                        .get(metadata.blob_body_offset.to_native() as usize..)
                        .unwrap_or_default(),
                );

                // Find a matching part
                'outer: for part in contents.parts.iter() {
                    match &part.body {
                        ArchivedMetadataPartType::Text => {
                            let text = match part.decode_contents(&raw_message) {
                                DecodedPartContent::Text(text) => text,
                                _ => unreachable!(),
                            };

                            if let Some(body) = generate_snippet(&text, terms, language, is_exact)
                            {
                                snippet.preview = body.into();
                                break;
                            }
                        }
                        ArchivedMetadataPartType::Html => {
                            let text = match part.decode_contents(&raw_message) {
                                DecodedPartContent::Text(html) => html_to_text(&html),
                                _ => unreachable!(),
                            };

                            if let Some(body) = generate_snippet(&text, terms, language, is_exact)
                            {
                                snippet.preview = body.into();
                                break;
                            }
                        }
                        ArchivedMetadataPartType::Message(message) => {
                            for part in metadata.contents[u16::from(message) as usize].parts.iter()
                            {
                                if let ArchivedMetadataPartType::Text
                                | ArchivedMetadataPartType::Html = part.body
                                {
                                    let text = match (part.decode_contents(&raw_message), &part.body)
                                    {
                                        (
                                            DecodedPartContent::Text(text),
                                            ArchivedMetadataPartType::Text,
                                        ) => text,
                                        (
                                            DecodedPartContent::Text(html),
                                            ArchivedMetadataPartType::Html,
                                        ) => html_to_text(&html).into(),
                                        _ => unreachable!(),
                                    };

                                    if let Some(body) =
                                        generate_snippet(&text, terms, language, is_exact)
                                    {
                                        snippet.preview = body.into();
                                        break 'outer;
                                    }
                                }
                            }
                        }
                        _ => (),
                    }
                }

                Ok(SnippetOutcome::Found(snippet))
            })
            .buffer_unordered(concurrency)
            .try_collect()
            .await?;

        for outcome in outcomes {
            match outcome {
                SnippetOutcome::Found(s) => response.list.push(s),
                SnippetOutcome::NotFound(id) => not_found.push(id),
            }
        }

        if !not_found.is_empty() {
            response.not_found = Some(not_found);
        }

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{StreamExt, TryStreamExt, stream};
    use std::time::Duration;
    // `tokio::time::Instant` is driven by the test runtime's virtual clock, so
    // under `start_paused = true` it advances with the mocked `sleep`s. Using
    // `std::time::Instant` here would measure wall-clock (~µs under auto-advance)
    // and pass even for a sequential implementation, making the test vacuous.
    use tokio::time::Instant;

    /// Verifies the stream primitive that `email_search_snippet` relies on:
    /// `stream::iter(...).map(future).buffer_unordered(N)` should run up to
    /// `N` futures concurrently, collapsing wall time from `n*delay` down to
    /// roughly `ceil(n/N) * delay`. This is a unit-level proof that swapping
    /// the previous sequential `for` loop for `buffer_unordered` actually
    /// pays off when each iteration awaits a non-trivial latency (e.g. an
    /// S3-style blob GET); the integration layer uses the same primitive
    /// over real blob futures.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn buffer_unordered_collapses_latency() {
        let per_delay = Duration::from_millis(100);
        let count: usize = 16;
        let concurrency: usize = 8;

        let started = Instant::now();
        let outcomes: Vec<Duration> = stream::iter(0..count)
            .map(|_| async move {
                tokio::time::sleep(per_delay).await;
                Ok::<_, std::convert::Infallible>(per_delay)
            })
            .buffer_unordered(concurrency)
            .try_collect()
            .await
            .expect("stream completed");
        let elapsed = started.elapsed();

        assert_eq!(outcomes.len(), count);

        // Sequential lower bound (with paused time): exactly count * per_delay.
        // Concurrent upper bound: a small multiple of ceil(count/concurrency) * per_delay.
        let waves = count.div_ceil(concurrency) as u32;
        let concurrent_target = per_delay * waves;
        let sequential_total = per_delay * count as u32;
        assert!(
            elapsed < sequential_total / 2,
            "elapsed {elapsed:?} not meaningfully lower than sequential {sequential_total:?}",
        );
        assert!(
            elapsed <= concurrent_target * 2,
            "elapsed {elapsed:?} exceeded ~2x concurrent target {concurrent_target:?}",
        );
    }
}
