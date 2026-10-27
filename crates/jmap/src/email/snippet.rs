/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use common::{
    Server,
    auth::AccessToken,
    storage::{
        batch::{DocumentBatch, Lookup, MAX_BATCH_BYTES, MAX_BATCH_DOCUMENTS},
        blob::FetchedBlob,
    },
};
use email::{
    cache::{
        MessageCacheFetch,
        email::{MessageAccess, MessageCacheAccess},
        mailbox::MailboxCacheAccess,
    },
    message::{
        messagedata::{EmailMessageData, MetadataForm},
        metadata::{PartKind, PartSource},
    },
};
use futures_util::StreamExt;
use jmap_proto::{
    method::{
        query::Filter,
        search_snippet::{GetSearchSnippetRequest, GetSearchSnippetResponse, SearchSnippet},
    },
    object::email::EmailFilter,
    request::MaybeInvalid,
};
use nlp::language::{Language, search_snippet::generate_snippet, stemmer::Stemmer};
use std::future::Future;
use store::{
    backend::MAX_TOKEN_LENGTH,
    search::{MAX_PLANNED_TOKENS, MAX_QUERY_LEAVES, MAX_QUERY_PLANNED_TOKENS},
};
use trc::AddContext;
use types::{acl::Acl, blob_hash::BlobHash, collection::Collection, id::Id};
use utils::cheeky_hash::{CheekyHash, CheekyHashSet};

/// Maximum number of blobs downloaded concurrently by `SearchSnippet/get`.
///
/// The blob store can be a remote object store (S3, R2) with a high per-request
/// latency, so fetching the blobs one by one makes large result pages slow.
const SNIPPET_BLOB_CONCURRENCY: usize = 8;

/// Above this many message bytes in a batch, the blobs are downloaded one by one,
/// as upstream does. Each download reserves read memory for the message (twice its
/// size with LZ4), so eight large messages in flight could exceed the memory budget
/// and fail the request (and concurrent reads) with a retry-later error.
const SNIPPET_PARALLEL_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// A requested id after the metadata pass, in request order.
enum SnippetEntry {
    Done(SearchSnippet),
    NotFound(MaybeInvalid<Id>),
    Fetch {
        snippet: SearchSnippet,
        blob_hash: BlobHash,
        size: u64,
    },
}

enum SnippetDownload {
    Done(SearchSnippet),
    NotFound(MaybeInvalid<Id>),
    Fetched {
        snippet: SearchSnippet,
        blob_hash: BlobHash,
        blob: trc::Result<Option<FetchedBlob>>,
    },
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
        let mut seen = CheekyHashSet::default();
        let mut planned = 0usize;
        let mut conditions = 0usize;
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
                        conditions += 1;
                        if conditions > MAX_QUERY_LEAVES {
                            return Err(too_many_terms());
                        }
                        let (text, language_) =
                            Language::detect(text, self.core.email.default_language);
                        language = language_;
                        if (text.starts_with('"') && text.ends_with('"'))
                            || (text.starts_with('\'') && text.ends_with('\''))
                        {
                            let before = terms.len();
                            terms.extend(
                                language
                                    .tokenize_text(&text, MAX_TOKEN_LENGTH)
                                    .take(MAX_PLANNED_TOKENS)
                                    .map(|token| token.word.into_owned()),
                            );
                            planned += terms.len() - before;
                            is_exact = true;
                        } else {
                            let mut condition_words = 0;
                            for token in Stemmer::new(&text, language, MAX_TOKEN_LENGTH) {
                                if !seen.insert(CheekyHash::new(token.word.as_bytes())) {
                                    continue;
                                }
                                terms.push(token.word.into_owned());
                                if let Some(stemmed_word) = token.stemmed_word
                                    && seen.insert(CheekyHash::new(stemmed_word.as_bytes()))
                                {
                                    terms.push(stemmed_word.into_owned());
                                }
                                condition_words += 1;
                                if condition_words == MAX_PLANNED_TOKENS {
                                    break;
                                }
                            }
                            planned += condition_words;
                        }
                        if planned > MAX_QUERY_PLANNED_TOKENS {
                            return Err(too_many_terms());
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
        let email_ids = request.email_ids.unwrap();
        if email_ids.len() > self.core.jmap.snippet_max_results {
            return Err(trc::JmapEvent::RequestTooLarge.into_err());
        }

        let account_id = request.account_id.document_id();
        let cached_messages = self
            .get_cached_messages(account_id)
            .await
            .caused_by(trc::location!())?;
        let access = if access_token.is_member(account_id) {
            MessageAccess::All
        } else {
            MessageAccess::Mailboxes(cached_messages.shared_mailboxes(access_token, Acl::ReadItems))
        };

        let mut response = GetSearchSnippetResponse {
            account_id: request.account_id,
            list: Vec::with_capacity(email_ids.len()),
            not_found: None,
        };
        let mut not_found = Vec::new();
        let mut batch = DocumentBatch::new(MAX_BATCH_BYTES);

        for chunk in email_ids.chunks(MAX_BATCH_DOCUMENTS) {
            batch.reset();
            if !terms.is_empty() {
                for email_id in chunk {
                    if let MaybeInvalid::Value(email_id) = email_id
                        && cached_messages
                            .email_by_id(&email_id.document_id())
                            .is_some_and(|message| access.allows(message))
                    {
                        batch.request(email_id.document_id());
                    }
                }
            }

            loop {
                self.read_metadata_batch(account_id, MetadataForm::Structure, &mut batch)
                    .await
                    .caused_by(trc::location!())?;

                // First pass: classify every requested id, in request order.
                let mut entries = Vec::with_capacity(chunk.len());
                for email_id in chunk {
                    let email_id = match email_id {
                        MaybeInvalid::Value(email_id) => *email_id,
                        invalid => {
                            if batch.is_first_round() {
                                entries.push(SnippetEntry::NotFound(invalid.clone()));
                            }
                            continue;
                        }
                    };
                    let document_id = email_id.document_id();
                    let mut snippet = SearchSnippet {
                        email_id,
                        subject: None,
                        preview: None,
                    };
                    if !cached_messages
                        .email_by_id(&document_id)
                        .is_some_and(|message| access.allows(message))
                    {
                        if batch.is_first_round() {
                            entries.push(SnippetEntry::NotFound(MaybeInvalid::Value(email_id)));
                        }
                        continue;
                    } else if terms.is_empty() {
                        entries.push(SnippetEntry::Done(snippet));
                        continue;
                    }
                    let structure = match batch.lookup(document_id) {
                        Lookup::Found(structure) => structure,
                        Lookup::Missing => {
                            entries.push(SnippetEntry::NotFound(MaybeInvalid::Value(email_id)));
                            continue;
                        }
                        Lookup::Skip => continue,
                    };
                    let metadata = structure.unarchive().caused_by(trc::location!())?;

                    // Add subject snippet
                    if let Some(subject) =
                        metadata.root().envelope().subject().and_then(|subject| {
                            generate_snippet(subject, &terms, language, is_exact)
                        })
                    {
                        snippet.subject = subject.into();
                    }

                    entries.push(SnippetEntry::Fetch {
                        snippet,
                        blob_hash: metadata.blob_hash(),
                        size: metadata.size() as u64,
                    });
                }

                // Second pass: download the blobs with bounded concurrency. `buffered`
                // yields results in request order, so the response order (and the
                // order of `notFound`) is the same as with sequential downloads, and
                // at most SNIPPET_BLOB_CONCURRENCY blobs are held in memory. Batches
                // with large messages fall back to sequential downloads.
                let fetch_bytes: u64 = entries
                    .iter()
                    .map(|entry| match entry {
                        SnippetEntry::Fetch { size, .. } => *size,
                        _ => 0,
                    })
                    .sum();
                let concurrency = if fetch_bytes <= SNIPPET_PARALLEL_MAX_BYTES {
                    SNIPPET_BLOB_CONCURRENCY
                } else {
                    1
                };
                let mut downloads = futures_util::stream::iter(entries)
                    .map(|entry| async move {
                        match entry {
                            SnippetEntry::Fetch {
                                snippet,
                                blob_hash,
                                size,
                            } => {
                                let blob =
                                    self.fetch_blob(&blob_hash, 0..usize::MAX, Some(size)).await;
                                SnippetDownload::Fetched {
                                    snippet,
                                    blob_hash,
                                    blob,
                                }
                            }
                            SnippetEntry::Done(snippet) => SnippetDownload::Done(snippet),
                            SnippetEntry::NotFound(id) => SnippetDownload::NotFound(id),
                        }
                    })
                    .buffered(concurrency);

                while let Some(download) = downloads.next().await {
                    let (mut snippet, blob_hash, blob) = match download {
                        SnippetDownload::Done(snippet) => {
                            response.list.push(snippet);
                            continue;
                        }
                        SnippetDownload::NotFound(id) => {
                            not_found.push(id);
                            continue;
                        }
                        SnippetDownload::Fetched {
                            snippet,
                            blob_hash,
                            blob,
                        } => (snippet, blob_hash, blob),
                    };
                    let email_id = snippet.email_id;
                    let Some(raw_body) = blob? else {
                        trc::event!(
                            Store(trc::StoreEvent::NotFound),
                            AccountId = account_id,
                            DocumentId = email_id.document_id(),
                            Collection = Collection::Email,
                            BlobId = blob_hash.as_slice(),
                            Details = "Blob not found.",
                            CausedBy = trc::location!(),
                        );

                        not_found.push(MaybeInvalid::Value(email_id));
                        continue;
                    };
                    let Lookup::Found(structure) = batch.lookup(email_id.document_id()) else {
                        // The batch does not change between the two passes, so the
                        // first pass already resolved this document as Found.
                        debug_assert!(false, "snippet batch lookup changed between passes");
                        continue;
                    };
                    let metadata = structure.unarchive().caused_by(trc::location!())?;
                    let root = metadata.root();
                    let raw = metadata.raw_message(None, &raw_body);
                    let source = PartSource::Raw(raw);

                    // Find a matching part
                    'outer: for part in root.parts() {
                        match part.kind() {
                            PartKind::Text | PartKind::Html => {
                                if let Some(body) = part.plain_text(&source).and_then(|text| {
                                    generate_snippet(&text, &terms, language, is_exact)
                                }) {
                                    snippet.preview = body.into();
                                    break;
                                }
                            }
                            PartKind::Message => {
                                let Some(nested) = part.nested() else {
                                    continue;
                                };
                                let Some(nested_source) = metadata.source(nested, raw) else {
                                    continue;
                                };
                                for part in nested.parts().filter(|part| part.is_text()) {
                                    if let Some(body) =
                                        part.plain_text(&nested_source).and_then(|text| {
                                            generate_snippet(&text, &terms, language, is_exact)
                                        })
                                    {
                                        snippet.preview = body.into();
                                        break 'outer;
                                    }
                                }
                            }
                            _ => (),
                        }
                    }

                    response.list.push(snippet);
                }
                drop(downloads);

                if !batch.next_round() {
                    break;
                }
            }
        }

        if !not_found.is_empty() {
            response.not_found = Some(not_found);
        }

        Ok(response)
    }
}

#[cold]
fn too_many_terms() -> trc::Error {
    trc::LimitEvent::SizeRequest
        .into_err()
        .details("Too many search terms in one query")
}
