use imap_proto::{self, MailboxDatum, Response};
use lazy_static::lazy_static;
use regex::Regex;
use std::collections::HashSet;

use async_std::prelude::*;
use async_std::stream::Stream;
use async_std::sync;

use super::error::{Error, ParseError, Result};
use super::types::*;
use crate::codec::ResponseData;

// FIXME: check tags when filtering

lazy_static! {
    static ref AUTH_RESP_REGEX: Regex = Regex::new("^\\+ (.*)\r\n").unwrap();
}

pub fn parse_authenticate_response(line: &str) -> Result<&str> {
    if let Some(cap) = AUTH_RESP_REGEX.captures_iter(line).next() {
        let data = cap.get(1).map(|x| x.as_str()).unwrap_or("");
        return Ok(data);
    }
    Err(Error::Parse(ParseError::Authentication(
        line.to_string(),
        None,
    )))
}

pub fn parse_names<'a, T: Stream<Item = ResponseData> + Unpin>(
    stream: &'a mut T,
    unsolicited: sync::Sender<UnsolicitedResponse>,
) -> impl Stream<Item = Result<Name<'a>>> + 'a {
    use futures::StreamExt;

    StreamExt::filter_map(
        StreamExt::take_while(stream, |res| match res.parsed() {
            Response::Done { .. } => futures::future::ready(false),
            _ => futures::future::ready(true),
        }),
        move |resp| {
            let unsolicited = unsolicited.clone();

            async move {
                match resp.parsed() {
                    Response::MailboxData(MailboxDatum::List {
                        flags,
                        delimiter,
                        name,
                    }) => Some(Ok(Name {
                        attributes: flags
                            .into_iter()
                            .map(|s| NameAttribute::from((*s).to_string()))
                            .collect(),
                        delimiter: (*delimiter).map(Into::into),
                        name: (*name).into(),
                    })),
                    _resp => match handle_unilateral(&resp, unsolicited).await {
                        Some(resp) => match resp.parsed() {
                            Response::Fetch(..) => None,
                            resp => Some(Err(resp.into())),
                        },
                        None => None,
                    },
                }
            }
        },
    )
}

pub fn parse_fetches<'a, T: Stream<Item = ResponseData> + Unpin>(
    stream: &'a mut T,
    unsolicited: sync::Sender<UnsolicitedResponse>,
) -> impl Stream<Item = Result<Fetch<'a>>> + 'a {
    use futures::StreamExt;

    StreamExt::filter_map(
        StreamExt::take_while(stream, |res| match res.parsed() {
            Response::Done { .. } => futures::future::ready(false),
            _ => futures::future::ready(true),
        }),
        move |resp| {
            let unsolicited = unsolicited.clone();

            async move {
                match resp.parsed() {
                    Response::Fetch(num, attrs) => {
                        let mut fetch = Fetch {
                            message: *num,
                            flags: vec![],
                            uid: None,
                            size: None,
                            fetch: Vec::new(), // FIXME: attrs.to_vec(),
                        };

                        // set some common fields eaglery
                        for attr in &fetch.fetch {
                            use imap_proto::AttributeValue;
                            match attr {
                                AttributeValue::Flags(flags) => {
                                    fetch.flags.extend(flags.iter().cloned().map(Flag::from));
                                }
                                AttributeValue::Uid(uid) => fetch.uid = Some(*uid),
                                AttributeValue::Rfc822Size(sz) => fetch.size = Some(*sz),
                                _ => {}
                            }
                        }
                        Some(Ok(fetch))
                    }
                    _ => match handle_unilateral(&resp, unsolicited).await {
                        Some(resp) => match resp.parsed() {
                            Response::Fetch(..) => None,
                            resp => Some(Err(resp.into())),
                        },
                        None => None,
                    },
                }
            }
        },
    )
}

pub fn parse_expunge<'a, T: Stream<Item = ResponseData> + Unpin>(
    stream: &'a mut T,
    unsolicited: sync::Sender<UnsolicitedResponse>,
) -> impl Stream<Item = Result<u32>> + 'a {
    use futures::StreamExt;

    StreamExt::filter_map(
        StreamExt::take_while(stream, |res| match res.parsed() {
            Response::Done { .. } => futures::future::ready(false),
            _ => futures::future::ready(true),
        }),
        move |resp| {
            let unsolicited = unsolicited.clone();

            async move {
                match resp.parsed() {
                    Response::Expunge(id) => Some(Ok(*id)),
                    _ => match handle_unilateral(&resp, unsolicited).await {
                        Some(resp) => match resp.parsed() {
                            Response::Fetch(..) => None,
                            resp => Some(Err(resp.into())),
                        },
                        None => None,
                    },
                }
            }
        },
    )
}

pub async fn parse_capabilities<'a, T: Stream<Item = ResponseData> + Unpin>(
    stream: &'a mut T,
    unsolicited: sync::Sender<UnsolicitedResponse>,
) -> Result<Capabilities> {
    let mut caps: HashSet<Capability> = HashSet::new();

    while let Some(resp) = stream
        .take_while(|res| match res.parsed() {
            Response::Done { .. } => false,
            _ => true,
        })
        .next()
        .await
    {
        match resp.parsed() {
            Response::Capabilities(cs) => {
                for c in cs {
                    caps.insert(Capability::from(c)); // TODO: avoid clone
                }
            }
            _ => {
                if let Some(resp) = handle_unilateral(&resp, unsolicited.clone()).await {
                    return Err(resp.parsed().into());
                }
            }
        }
    }

    Ok(Capabilities(caps))
}

pub async fn parse_noop<T: Stream<Item = ResponseData> + Unpin>(
    stream: &mut T,
    unsolicited: sync::Sender<UnsolicitedResponse>,
) -> Result<()> {
    let s = futures::StreamExt::filter_map(
        futures::StreamExt::take_while(stream, |res| match res.parsed() {
            Response::Done { .. } => futures::future::ready(false),
            _ => futures::future::ready(true),
        }),
        move |resp| {
            let unsolicited = unsolicited.clone();

            async move {
                if let Some(resp) = handle_unilateral(&resp, unsolicited).await {
                    return Some(Err(resp.parsed().into()));
                }
                None
            }
        },
    );
    s.collect::<Result<()>>().await?;

    Ok(())
}

pub async fn parse_mailbox<T: Stream<Item = ResponseData> + Unpin>(
    stream: &mut T,
    unsolicited: sync::Sender<UnsolicitedResponse>,
) -> Result<Mailbox> {
    let mut mailbox = Mailbox::default();

    while let Some(resp) = stream
        .take_while(|res| match res.parsed() {
            Response::Done { .. } => false,
            _ => true,
        })
        .next()
        .await
    {
        match resp.parsed() {
            Response::Data { status, code, .. } => {
                if let imap_proto::Status::Ok = status {
                } else {
                    // how can this happen for a Response::Data?
                    unreachable!();
                }

                use imap_proto::ResponseCode;
                match code {
                    Some(ResponseCode::UidValidity(uid)) => {
                        mailbox.uid_validity = Some(*uid);
                    }
                    Some(ResponseCode::UidNext(unext)) => {
                        mailbox.uid_next = Some(*unext);
                    }
                    Some(ResponseCode::Unseen(n)) => {
                        mailbox.unseen = Some(*n);
                    }
                    Some(ResponseCode::PermanentFlags(flags)) => {
                        mailbox
                            .permanent_flags
                            .extend(flags.into_iter().map(|s| (*s).to_string()).map(Flag::from));
                    }
                    _ => {}
                }
            }
            Response::MailboxData(m) => match m {
                MailboxDatum::Status { mailbox, status } => {
                    unsolicited
                        .send(UnsolicitedResponse::Status {
                            mailbox: (*mailbox).into(),
                            attributes: Vec::new(), // FIXME: status,
                        })
                        .await;
                }
                MailboxDatum::Exists(e) => {
                    mailbox.exists = *e;
                }
                MailboxDatum::Recent(r) => {
                    mailbox.recent = *r;
                }
                MailboxDatum::Flags(flags) => {
                    mailbox
                        .flags
                        .extend(flags.into_iter().map(|s| (*s).to_string()).map(Flag::from));
                }
                MailboxDatum::List { .. } => {}
            },
            Response::Expunge(n) => {
                unsolicited.send(UnsolicitedResponse::Expunge(*n)).await;
            }
            _ => {
                return Err(resp.parsed().into());
            }
        }
    }

    Ok(mailbox)
}

pub async fn parse_ids<T: Stream<Item = ResponseData> + Unpin>(
    stream: &mut T,
    unsolicited: sync::Sender<UnsolicitedResponse>,
) -> Result<HashSet<u32>> {
    let mut ids: HashSet<u32> = HashSet::new();

    while let Some(resp) = stream
        .take_while(|res| match res.parsed() {
            Response::Done { .. } => false,
            _ => true,
        })
        .next()
        .await
    {
        match resp.parsed() {
            Response::IDs(cs) => {
                for c in cs {
                    ids.insert(*c);
                }
            }
            _ => {
                if let Some(resp) = handle_unilateral(&resp, unsolicited.clone()).await {
                    return Err(resp.parsed().into());
                }
            }
        }
    }

    Ok(ids)
}

// check if this is simply a unilateral server response
// (see Section 7 of RFC 3501):
async fn handle_unilateral<'a>(
    res: &'a ResponseData,
    unsolicited: sync::Sender<UnsolicitedResponse>,
) -> Option<&'a ResponseData> {
    match res.parsed() {
        Response::MailboxData(MailboxDatum::Status { mailbox, status }) => {
            unsolicited
                .send(UnsolicitedResponse::Status {
                    mailbox: (*mailbox).into(),
                    attributes: Vec::new(), // status, FIXME
                })
                .await;
        }
        Response::MailboxData(MailboxDatum::Recent(n)) => {
            unsolicited.send(UnsolicitedResponse::Recent(*n)).await;
        }
        Response::MailboxData(MailboxDatum::Exists(n)) => {
            unsolicited.send(UnsolicitedResponse::Exists(*n)).await;
        }
        Response::Expunge(n) => {
            unsolicited.send(UnsolicitedResponse::Expunge(*n)).await;
        }
        _res => {
            return Some(res);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_stream(data: &[&str]) -> Vec<ResponseData> {
        data.iter()
            .map(|line| match imap_proto::parse_response(line.as_bytes()) {
                Ok((remaining, response)) => {
                    let response = unsafe { std::mem::transmute(response) };
                    assert_eq!(remaining.len(), 0);

                    ResponseData {
                        raw: line.as_bytes().to_vec().into(),
                        response,
                    }
                }
                Err(err) => panic!("invalid input: {:?}", err),
            })
            .collect()
    }

    #[test]
    fn parse_capability_test() {
        async_std::task::block_on(async move {
            let expected_capabilities =
                vec!["IMAP4rev1", "STARTTLS", "AUTH=GSSAPI", "LOGINDISABLED"];
            let responses = input_stream(&vec![
                "* CAPABILITY IMAP4rev1 STARTTLS AUTH=GSSAPI LOGINDISABLED\r\n",
            ]);

            let mut stream = async_std::stream::from_iter(responses);
            let (send, recv) = sync::channel(10);
            let capabilities = parse_capabilities(&mut stream, send).await.unwrap();
            // shouldn't be any unexpected responses parsed
            assert!(recv.is_empty());
            assert_eq!(capabilities.len(), 4);
            for e in expected_capabilities {
                assert!(capabilities.has_str(e));
            }
        });
    }

    #[test]
    fn parse_capability_case_insensitive_test() {
        async_std::task::block_on(async move {
            // Test that "IMAP4REV1" (instead of "IMAP4rev1") is accepted
            let expected_capabilities = vec!["IMAP4rev1", "STARTTLS"];
            let responses = input_stream(&vec!["* CAPABILITY IMAP4REV1 STARTTLS\r\n"]);
            let mut stream = async_std::stream::from_iter(responses);

            let (send, recv) = sync::channel(10);
            let capabilities = parse_capabilities(&mut stream, send).await.unwrap();

            // shouldn't be any unexpected responses parsed
            assert!(recv.is_empty());
            assert_eq!(capabilities.len(), 2);
            for e in expected_capabilities {
                assert!(capabilities.has_str(e));
            }
        });
    }

    #[test]
    #[should_panic]
    fn parse_capability_invalid_test() {
        async_std::task::block_on(async move {
            let (send, recv) = sync::channel(10);
            let responses = input_stream(&vec![
                "* JUNK IMAP4rev1 STARTTLS AUTH=GSSAPI LOGINDISABLED\r\n",
            ]);
            let mut stream = async_std::stream::from_iter(responses);

            parse_capabilities(&mut stream, send.clone()).await.unwrap();
            assert!(recv.is_empty());
        });
    }

    #[test]
    fn parse_names_test() {
        async_std::task::block_on(async move {
            let (send, recv) = sync::channel(10);
            let responses = input_stream(&vec!["* LIST (\\HasNoChildren) \".\" \"INBOX\"\r\n"]);
            let mut stream = async_std::stream::from_iter(responses);

            let names: Vec<_> = parse_names(&mut stream, send)
                .collect::<Result<Vec<Name<'_>>>>()
                .await
                .unwrap();
            assert!(recv.is_empty());
            assert_eq!(names.len(), 1);
            assert_eq!(
                names[0].attributes(),
                &[NameAttribute::from("\\HasNoChildren")]
            );
            assert_eq!(names[0].delimiter(), Some("."));
            assert_eq!(names[0].name(), "INBOX");
        });
    }

    #[test]
    fn parse_fetches_empty() {
        async_std::task::block_on(async move {
            let (send, recv) = sync::channel(10);
            let responses = input_stream(&vec![]);
            let mut stream = async_std::stream::from_iter(responses);

            let fetches = parse_fetches(&mut stream, send)
                .collect::<Result<Vec<_>>>()
                .await
                .unwrap();
            assert!(recv.is_empty());
            assert!(fetches.is_empty());
        });
    }

    #[test]
    fn parse_fetches_test() {
        async_std::task::block_on(async move {
            let (send, recv) = sync::channel(10);
            let responses = input_stream(&vec![
                "* 24 FETCH (FLAGS (\\Seen) UID 4827943)\r\n",
                "* 25 FETCH (FLAGS (\\Seen))\r\n",
            ]);
            let mut stream = async_std::stream::from_iter(responses);

            let fetches = parse_fetches(&mut stream, send)
                .collect::<Result<Vec<_>>>()
                .await
                .unwrap();
            assert!(recv.is_empty());

            assert_eq!(fetches.len(), 2);
            assert_eq!(fetches[0].message, 24);
            assert_eq!(fetches[0].flags(), &[Flag::Seen]);
            assert_eq!(fetches[0].uid, Some(4827943));
            assert_eq!(fetches[0].body(), None);
            assert_eq!(fetches[0].header(), None);
            assert_eq!(fetches[1].message, 25);
            assert_eq!(fetches[1].flags(), &[Flag::Seen]);
            assert_eq!(fetches[1].uid, None);
            assert_eq!(fetches[1].body(), None);
            assert_eq!(fetches[1].header(), None);
        });
    }

    #[test]
    fn parse_fetches_w_unilateral() {
        // https://github.com/mattnenterprise/rust-imap/issues/81
        async_std::task::block_on(async move {
            let (send, recv) = sync::channel(10);
            let responses = input_stream(&vec!["* 37 FETCH (UID 74)\r\n", "* 1 RECENT\r\n"]);
            let mut stream = async_std::stream::from_iter(responses);

            let fetches = parse_fetches(&mut stream, send)
                .collect::<Result<Vec<_>>>()
                .await
                .unwrap();
            assert_eq!(recv.recv().await, Some(UnsolicitedResponse::Recent(1)));

            assert_eq!(fetches.len(), 1);
            assert_eq!(fetches[0].message, 37);
            assert_eq!(fetches[0].uid, Some(74));
        });
    }

    #[test]
    fn parse_names_w_unilateral() {
        async_std::task::block_on(async move {
            let (send, recv) = sync::channel(10);
            let responses = input_stream(&vec![
                "* LIST (\\HasNoChildren) \".\" \"INBOX\"\r\n",
                "* 4 EXPUNGE\r\n",
            ]);
            let mut stream = async_std::stream::from_iter(responses);

            let names = parse_names(&mut stream, send)
                .collect::<Result<Vec<_>>>()
                .await
                .unwrap();

            assert_eq!(recv.recv().await, Some(UnsolicitedResponse::Expunge(4)));

            assert_eq!(names.len(), 1);
            assert_eq!(
                names[0].attributes(),
                &[NameAttribute::from("\\HasNoChildren")]
            );
            assert_eq!(names[0].delimiter(), Some("."));
            assert_eq!(names[0].name(), "INBOX");
        });
    }

    #[test]
    fn parse_capabilities_w_unilateral() {
        async_std::task::block_on(async move {
            let (send, recv) = sync::channel(10);
            let responses = input_stream(&vec![
                "* CAPABILITY IMAP4rev1 STARTTLS AUTH=GSSAPI LOGINDISABLED\r\n",
                "* STATUS dev.github (MESSAGES 10 UIDNEXT 11 UIDVALIDITY 1408806928 UNSEEN 0)\r\n",
                "* 4 EXISTS\r\n",
            ]);
            let mut stream = async_std::stream::from_iter(responses);

            let expected_capabilities =
                vec!["IMAP4rev1", "STARTTLS", "AUTH=GSSAPI", "LOGINDISABLED"];

            let capabilities = parse_capabilities(&mut stream, send).await.unwrap();

            assert_eq!(capabilities.len(), 4);
            for e in expected_capabilities {
                assert!(capabilities.has_str(e));
            }

            assert_eq!(
                recv.recv().await.unwrap(),
                UnsolicitedResponse::Status {
                    mailbox: "dev.github".to_string(),
                    attributes: vec![
                        StatusAttribute::Messages(10),
                        StatusAttribute::UidNext(11),
                        StatusAttribute::UidValidity(1408806928),
                        StatusAttribute::Unseen(0)
                    ]
                }
            );
            assert_eq!(recv.recv().await.unwrap(), UnsolicitedResponse::Exists(4));
        });
    }

    #[test]
    fn parse_ids_w_unilateral() {
        async_std::task::block_on(async move {
            let (send, recv) = sync::channel(10);
            let responses = input_stream(&vec![
                "* SEARCH 23 42 4711\r\n",
                "* 1 RECENT\r\n",
                "* STATUS INBOX (MESSAGES 10 UIDNEXT 11 UIDVALIDITY 1408806928 UNSEEN 0)\r\n",
            ]);
            let mut stream = async_std::stream::from_iter(responses);

            let ids = parse_ids(&mut stream, send).await.unwrap();

            assert_eq!(ids, [23, 42, 4711].iter().cloned().collect());

            assert_eq!(recv.recv().await.unwrap(), UnsolicitedResponse::Recent(1));
            assert_eq!(
                recv.recv().await.unwrap(),
                UnsolicitedResponse::Status {
                    mailbox: "INBOX".to_string(),
                    attributes: vec![
                        StatusAttribute::Messages(10),
                        StatusAttribute::UidNext(11),
                        StatusAttribute::UidValidity(1408806928),
                        StatusAttribute::Unseen(0)
                    ]
                }
            );
        });
    }

    #[test]
    fn parse_ids_test() {
        async_std::task::block_on(async move {
            let (send, recv) = sync::channel(10);
            let responses = input_stream(&vec![
                "* SEARCH 1600 1698 1739 1781 1795 1885 1891 1892 1893 1898 1899 1901 1911 1926 1932 1933 1993 1994 2007 2032 2033 2041 2053 2062 2063 2065 2066 2072 2078 2079 2082 2084 2095 2100 2101 2102 2103 2104 2107 2116 2120 2135 2138 2154 2163 2168 2172 2189 2193 2198 2199 2205 2212 2213 2221 2227 2267 2275 2276 2295 2300 2328 2330 2332 2333 2334\r\n",
                "* SEARCH 2335 2336 2337 2338 2339 2341 2342 2347 2349 2350 2358 2359 2362 2369 2371 2372 2373 2374 2375 2376 2377 2378 2379 2380 2381 2382 2383 2384 2385 2386 2390 2392 2397 2400 2401 2403 2405 2409 2411 2414 2417 2419 2420 2424 2426 2428 2439 2454 2456 2467 2468 2469 2490 2515 2519 2520 2521\r\n",
            ]);
            let mut stream = async_std::stream::from_iter(responses);

            let ids = parse_ids(&mut stream, send).await.unwrap();

            assert!(recv.is_empty());
            let ids: HashSet<u32> = ids.iter().cloned().collect();
            assert_eq!(
                ids,
                [
                    1600, 1698, 1739, 1781, 1795, 1885, 1891, 1892, 1893, 1898, 1899, 1901, 1911,
                    1926, 1932, 1933, 1993, 1994, 2007, 2032, 2033, 2041, 2053, 2062, 2063, 2065,
                    2066, 2072, 2078, 2079, 2082, 2084, 2095, 2100, 2101, 2102, 2103, 2104, 2107,
                    2116, 2120, 2135, 2138, 2154, 2163, 2168, 2172, 2189, 2193, 2198, 2199, 2205,
                    2212, 2213, 2221, 2227, 2267, 2275, 2276, 2295, 2300, 2328, 2330, 2332, 2333,
                    2334, 2335, 2336, 2337, 2338, 2339, 2341, 2342, 2347, 2349, 2350, 2358, 2359,
                    2362, 2369, 2371, 2372, 2373, 2374, 2375, 2376, 2377, 2378, 2379, 2380, 2381,
                    2382, 2383, 2384, 2385, 2386, 2390, 2392, 2397, 2400, 2401, 2403, 2405, 2409,
                    2411, 2414, 2417, 2419, 2420, 2424, 2426, 2428, 2439, 2454, 2456, 2467, 2468,
                    2469, 2490, 2515, 2519, 2520, 2521
                ]
                .iter()
                .cloned()
                .collect()
            );
        });
    }

    #[test]
    fn parse_ids_search() {
        async_std::task::block_on(async move {
            let (send, recv) = sync::channel(10);
            let responses = input_stream(&vec!["* SEARCH\r\n"]);
            let mut stream = async_std::stream::from_iter(responses);

            let ids = parse_ids(&mut stream, send).await.unwrap();

            assert!(recv.is_empty());
            let ids: HashSet<u32> = ids.iter().cloned().collect();
            assert_eq!(ids, HashSet::<u32>::new());
        });
    }
}