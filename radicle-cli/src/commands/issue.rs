#![allow(clippy::or_fun_call)]
use std::ffi::OsString;
use std::str::FromStr;

use anyhow::{anyhow, Context as _};

use crate::terminal as term;
use crate::terminal::args::{Args, Error, Help};

use radicle::cob::thread::{self, CommentId};
use radicle::cob::Timestamp;
use serde::Serialize;
use serde_json::json;

use radicle::cob;
use radicle::cob::common::{Reaction, Tag};
use radicle::cob::issue;
use radicle::cob::issue::{CloseReason, IssueId, Issues, State};
use radicle::identity::PublicKey;
use radicle::storage::WriteStorage;

pub const HELP: Help = Help {
    name: "issue",
    description: "Manage issues",
    version: env!("CARGO_PKG_VERSION"),
    usage: r#"
Usage

    rad issue
    rad issue new [--title <title>] [--description <text>]
    rad issue show <id>
    rad issue state <id> [--closed | --open | --solved]
    rad issue delete <id>
    rad issue react <id> [--emoji <char>]
    rad issue list [--assigned <key>]

Options

    --help      Print help
    --payload   Print JSON output like HTTP API
"#,
};

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct Metadata {
    title: String,
    labels: Vec<Tag>,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub enum OperationName {
    Create,
    Delete,
    #[default]
    List,
    React,
    Show,
    State,
}

/// Command line Peer argument.
#[derive(Default, Debug, PartialEq, Eq)]
pub enum Assigned {
    #[default]
    Me,
    Peer(cob::ActorId),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Operation {
    Create {
        title: Option<String>,
        description: Option<String>,
    },
    Show {
        id: IssueId,
        json: Option<bool>,
    },
    State {
        id: IssueId,
        state: State,
    },
    Delete {
        id: IssueId,
    },
    React {
        id: IssueId,
        reaction: Reaction,
    },
    List {
        assigned: Option<Assigned>,
    },
}

#[derive(Debug)]
pub struct Options {
    pub op: Operation,
}

impl Args for Options {
    fn from_args(args: Vec<OsString>) -> anyhow::Result<(Self, Vec<OsString>)> {
        use lexopt::prelude::*;

        let mut parser = lexopt::Parser::from_args(args);
        let mut op: Option<OperationName> = None;
        let mut id: Option<IssueId> = None;
        let mut assigned: Option<Assigned> = None;
        let mut title: Option<String> = None;
        let mut reaction: Option<Reaction> = None;
        let mut description: Option<String> = None;
        let mut state: Option<State> = None;
        let mut json_out: Option<bool> = Some(false);

        while let Some(arg) = parser.next()? {
            match arg {
                Long("help") => {
                    return Err(Error::Help.into());
                }
                Long("json") => {
                    json_out = Some(true);
                }
                Long("title") if op == Some(OperationName::Create) => {
                    title = Some(parser.value()?.to_string_lossy().into());
                }
                Long("closed") if op == Some(OperationName::State) => {
                    state = Some(State::Closed {
                        reason: CloseReason::Other,
                    });
                }
                Long("open") if op == Some(OperationName::State) => {
                    state = Some(State::Open);
                }
                Long("solved") if op == Some(OperationName::State) => {
                    state = Some(State::Closed {
                        reason: CloseReason::Solved,
                    });
                }
                Long("reaction") if op == Some(OperationName::React) => {
                    if let Some(emoji) = parser.value()?.to_str() {
                        reaction =
                            Some(Reaction::from_str(emoji).map_err(|_| anyhow!("invalid emoji"))?);
                    }
                }
                Long("description") if op == Some(OperationName::Create) => {
                    description = Some(parser.value()?.to_string_lossy().into());
                }
                Long("assigned") | Short('a') if assigned.is_none() => {
                    if let Ok(val) = parser.value() {
                        let val = val.to_string_lossy();
                        let Ok(peer) = cob::ActorId::from_str(&val) else {
                            return Err(anyhow!("invalid peer ID '{}'", val));
                        };
                        assigned = Some(Assigned::Peer(peer));
                    } else {
                        assigned = Some(Assigned::Me);
                    }
                }
                Value(val) if op.is_none() => match val.to_string_lossy().as_ref() {
                    "n" | "new" => op = Some(OperationName::Create),
                    "c" | "show" => op = Some(OperationName::Show),
                    "s" | "state" => op = Some(OperationName::State),
                    "d" | "delete" => op = Some(OperationName::Delete),
                    "l" | "list" => op = Some(OperationName::List),
                    "r" | "react" => op = Some(OperationName::React),

                    unknown => anyhow::bail!("unknown operation '{}'", unknown),
                },
                Value(val) if op.is_some() => {
                    let val = val
                        .to_str()
                        .ok_or_else(|| anyhow!("issue id specified is not UTF-8"))?;

                    id = Some(
                        IssueId::from_str(val)
                            .map_err(|_| anyhow!("invalid issue id '{}'", val))?,
                    );
                }
                _ => {
                    return Err(anyhow!(arg.unexpected()));
                }
            }
        }

        let op = match op.unwrap_or_default() {
            OperationName::Create => Operation::Create { title, description },
            OperationName::Show => Operation::Show {
                id: id.ok_or_else(|| anyhow!("an issue id must be provided"))?,
                json: json_out,
            },
            OperationName::State => Operation::State {
                id: id.ok_or_else(|| anyhow!("an issue id must be provided"))?,
                state: state.ok_or_else(|| anyhow!("a state operation must be provided"))?,
            },
            OperationName::React => Operation::React {
                id: id.ok_or_else(|| anyhow!("an issue id must be provided"))?,
                reaction: reaction.ok_or_else(|| anyhow!("a reaction emoji must be provided"))?,
            },
            OperationName::Delete => Operation::Delete {
                id: id.ok_or_else(|| anyhow!("an issue id to remove must be provided"))?,
            },
            OperationName::List => Operation::List { assigned },
        };

        Ok((Options { op }, vec![]))
    }
}

pub fn run(options: Options, ctx: impl term::Context) -> anyhow::Result<()> {
    let profile = ctx.profile()?;
    let signer = term::signer(&profile)?;
    let storage = &profile.storage;
    let (_, id) = radicle::rad::cwd()?;
    let repo = storage.repository(id)?;
    let mut issues = Issues::open(*signer.public_key(), &repo)?;

    match options.op {
        Operation::Create {
            title: Some(title),
            description: Some(description),
        } => {
            issues.create(title, description, &[], &signer)?;
        }
        Operation::Show { id, json } => {
            let error_message = "No issue with the given ID exists";
            let mut _output: String = String::from(error_message);

            if json == Some(true) {
                let json_out = json!({ "errors": error_message });
                _output = json_out.to_string();
            }

            let issue = issues.get(&id)?.context(_output)?;
            show_issue(&issue, id, json)?;
        }
        Operation::State { id, state } => {
            let mut issue = issues.get_mut(&id)?;
            issue.lifecycle(state, &signer)?;
        }
        Operation::React { id, reaction } => {
            if let Ok(mut issue) = issues.get_mut(&id) {
                let comment_id = term::comment_select(&issue).unwrap();
                issue.react(comment_id, reaction, &signer)?;
            }
        }
        Operation::Create { title, description } => {
            let meta = Metadata {
                title: title.unwrap_or("Enter a title".to_owned()),
                labels: vec![],
            };
            let yaml = serde_yaml::to_string(&meta)?;
            let doc = format!(
                "{}---\n\n{}",
                yaml,
                description.unwrap_or("Enter a description...".to_owned())
            );

            if let Some(text) = term::Editor::new().edit(&doc)? {
                let mut meta = String::new();
                let mut frontmatter = false;
                let mut lines = text.lines();

                while let Some(line) = lines.by_ref().next() {
                    if line.trim() == "---" {
                        if frontmatter {
                            break;
                        } else {
                            frontmatter = true;
                            continue;
                        }
                    }
                    if frontmatter {
                        meta.push_str(line);
                        meta.push('\n');
                    }
                }

                let description: String = lines.collect::<Vec<&str>>().join("\n");
                let meta: Metadata =
                    serde_yaml::from_str(&meta).context("failed to parse yaml front-matter")?;

                issues.create(
                    &meta.title,
                    description.trim(),
                    meta.labels.as_slice(),
                    &signer,
                )?;
            }
        }
        Operation::List { assigned } => {
            let assignee = match assigned {
                Some(Assigned::Me) => Some(*profile.id()),
                Some(Assigned::Peer(id)) => Some(id),
                None => None,
            };

            let mut t = term::Table::new(term::table::TableOptions::default());
            for result in issues.all()? {
                let (id, issue, _) = result?;
                let assigned: Vec<_> = issue.assigned().collect();

                if Some(true) == assignee.map(|a| !assigned.contains(&&a)) {
                    continue;
                }

                let assigned: String = assigned
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                t.push([
                    id.to_string(),
                    format!("{:?}", issue.title()),
                    assigned.to_string(),
                ]);
            }
            t.render();
        }
        Operation::Delete { id } => {
            issues.remove(&id)?;
        }
    }

    Ok(())
}

#[derive(Serialize)]
struct Author {
    id: PublicKey,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Comment {
    author: Author,
    body: String,
    reactions: [String; 0],
    timestamp: Timestamp,
    reply_to: Option<CommentId>,
}
//
#[derive(Serialize)]
struct Comments(Vec<Comment>);

impl<'a> FromIterator<(&'a CommentId, &'a thread::Comment)> for Comments {
    fn from_iter<I: IntoIterator<Item = (&'a CommentId, &'a thread::Comment)>>(iter: I) -> Self {
        let mut comments = Vec::new();

        for (_, comment) in iter {
            comments.push(Comment {
                author: Author {
                    id: comment.author(),
                },
                body: comment.body().to_owned(),
                reactions: [],
                timestamp: comment.timestamp(),
                reply_to: comment.reply_to(),
            });
        }

        Comments(comments)
    }
}

fn show_issue(
    issue: &issue::Issue,
    issue_id: IssueId,
    json_output: Option<bool>,
) -> anyhow::Result<()> {
    if json_output == Some(true) {
        term::print(json!({
            "id": issue_id.to_string(),
            "author": issue.author(),
            "title": issue.title(),
            "description": issue.description(),
            "discussion": issue.comments().collect::<Comments>(),
            "tags": issue.tags().collect::<Vec<_>>(),
            "state": issue.state()
        }))
    } else {
        term::info!("title: {}", issue.title());
        term::info!("state: {}", issue.state());

        let tags: Vec<String> = issue.tags().cloned().map(|t| t.into()).collect();
        term::info!("tags: {}", tags.join(", "));

        let assignees: Vec<String> = issue.assigned().map(|a| a.to_string()).collect();
        term::info!("assignees: {}", assignees.join(", "));

        term::info!("{}", issue.description().unwrap_or(""));
    }
    Ok(())
}
