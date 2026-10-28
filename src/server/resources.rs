//! Resources (`gitrs://` commits, files and diffs) and argument completion.
//!
//! Thin mapping between rmcp's types and `tools::resources`, which holds the logic.

use std::collections::HashMap;

use rmcp::{
    ErrorData as McpError,
    model::{
        CompleteRequestParams, CompleteResult, CompletionInfo, ListResourceTemplatesResult,
        ListResourcesResult, ReadResourceResult, Resource, ResourceContents, ResourceTemplate,
    },
};

use super::{
    GitServer,
    repos::{split_repo, with_repo},
};
use crate::tools::resources::{self, Body};

impl GitServer {
    /// The fixed resources of every served repository; those of other repositories than the
    /// default carry `?repo=<name>`.
    pub(super) async fn resource_list(&self) -> Result<ListResourcesResult, McpError> {
        let mut listed = Vec::new();
        for (index, (name, handle)) in self.repos.entries().into_iter().enumerate() {
            let fixed = handle
                .run(|r| Ok(resources::fixed(r)))
                .await
                .map_err(|e| McpError::internal_error(e, None))?;
            let repo = (index > 0).then_some(name.as_str());
            for r in fixed {
                let (resource_name, title) = match repo {
                    Some(repo) => (
                        format!("{}-{repo}", r.name),
                        format!("{} ({repo})", r.title),
                    ),
                    None => (r.name.to_owned(), r.title.to_owned()),
                };
                listed.push(
                    Resource::new(with_repo(r.uri, repo), resource_name)
                        .with_title(title)
                        .with_description(r.description)
                        .with_mime_type(r.mime_type),
                );
            }
        }
        Ok(ListResourcesResult::with_all_items(listed))
    }

    /// The templates, each taking an optional `?repo=<name>` for another served repository.
    pub(super) fn resource_templates(&self) -> ListResourceTemplatesResult {
        let templates = resources::TEMPLATES
            .iter()
            .map(|t| {
                let template = ResourceTemplate::new(
                    format!("{}{{?repo}}", t.uri_template),
                    t.name,
                )
                .with_title(t.title)
                .with_description(format!(
                    "{} Add `?repo=<name>` to read from another served repository (list_repos).",
                    t.description
                ));
                match t.mime_type {
                    Some(mime) => template.with_mime_type(mime),
                    None => template,
                }
            })
            .collect();
        ListResourceTemplatesResult::with_all_items(templates)
    }

    pub(super) async fn resource_read(&self, uri: String) -> Result<ReadResourceResult, McpError> {
        let not_found = |e| McpError::resource_not_found(e, None);
        let (base, repo) = split_repo(&uri).map_err(not_found)?;
        let request = base.to_owned();
        let contents = self
            .repo(repo)
            .map_err(not_found)?
            .run(move |r| resources::read(r, &request))
            .await
            .map_err(not_found)?;
        let content = match contents.body {
            Body::Text(text) => ResourceContents::text(text, uri),
            Body::Base64(data) => ResourceContents::blob(data, uri),
        }
        .with_mime_type(contents.mime_type);
        Ok(ReadResourceResult::new(vec![content]))
    }

    /// Completion is a typing aid, so a context that doesn't resolve (a revision still being typed)
    /// gives no values rather than an error. `repo` completes from the served repositories, and a
    /// `repo` in the context picks the repository other arguments complete in.
    pub(super) async fn completion(&self, request: CompleteRequestParams) -> CompleteResult {
        let name = request.argument.name;
        let value = request.argument.value;
        let context: HashMap<String, String> = request
            .context
            .and_then(|c| c.arguments)
            .unwrap_or_default();
        let found = if name == "repo" {
            resources::matching(self.repos.names(), &value)
        } else {
            match self.repo(context.get("repo").cloned()) {
                Ok(handle) => handle
                    .run(move |r| resources::complete(r, &name, &value, &context))
                    .await
                    .unwrap_or_default(),
                Err(_) => resources::Completion::default(),
            }
        };
        let has_more = found.total > found.values.len();
        let total = u32::try_from(found.total).ok();
        let info =
            CompletionInfo::with_pagination(found.values, total, has_more).unwrap_or_default();
        CompleteResult::new(info)
    }
}

#[cfg(test)]
mod tests {
    use rmcp::{
        ServerHandler,
        model::{ArgumentInfo, CompletionContext, Reference},
    };

    use super::*;
    use crate::{
        repo::RepoHandle,
        server::Policy,
        tools::testutil::{fixture_repo, write},
    };

    fn server(path: &std::path::Path) -> GitServer {
        GitServer::new(
            RepoHandle::open_isolated(path),
            Policy {
                allow_write: false,
                watch_interval: None,
            },
        )
    }

    fn complete(name: &str, value: &str, context: &[(&str, &str)]) -> CompleteRequestParams {
        let mut request = CompleteRequestParams::new(
            Reference::for_resource("gitrs://file/{rev}/{+path}"),
            ArgumentInfo::new(name, value),
        );
        if !context.is_empty() {
            let arguments = context
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            request = request.with_context(CompletionContext::with_arguments(arguments));
        }
        request
    }

    #[tokio::test]
    async fn resources_are_listed_and_read() {
        let (dir, _) = fixture_repo();
        let server = server(dir.path());
        let capabilities = server.get_info().capabilities;
        assert!(capabilities.resources.is_some() && capabilities.completions.is_some());

        let listed = server.resource_list().await.unwrap().resources;
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].uri, "gitrs://commit/HEAD");
        assert_eq!(listed[0].mime_type.as_deref(), Some("text/x-diff"));
        let templates = server.resource_templates().resource_templates;
        assert_eq!(templates.len(), 3);
        assert!(
            templates.iter().any(|t| t.mime_type.is_none()),
            "files vary"
        );

        let read = server
            .resource_read("gitrs://file/HEAD/a.txt".into())
            .await
            .unwrap();
        let json = serde_json::to_value(&read.contents).unwrap();
        assert_eq!(json[0]["text"], "one\ntwo\n");
        assert_eq!(json[0]["mimeType"], "text/plain");
        assert_eq!(json[0]["uri"], "gitrs://file/HEAD/a.txt");

        write(dir.path(), "bin.dat", b"\0\x01");
        let blob = server
            .resource_read("gitrs://file/worktree/bin.dat".into())
            .await
            .unwrap();
        let json = serde_json::to_value(&blob.contents).unwrap();
        assert_eq!(json[0]["blob"], "AAE=");

        let err = server
            .resource_read("gitrs://commit/nope".into())
            .await
            .unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::RESOURCE_NOT_FOUND);
        assert!(err.message.contains("nope"));
    }

    #[tokio::test]
    async fn completion_reports_paging_and_hides_errors() {
        let (dir, _) = fixture_repo();
        let server = server(dir.path());
        let found = server
            .completion(complete("rev", "f", &[]))
            .await
            .completion;
        assert_eq!(found.values, ["feature"]);
        assert_eq!((found.total, found.has_more), (Some(1), Some(false)));

        let paths = server
            .completion(complete("path", "", &[("rev", "HEAD~1")]))
            .await
            .completion;
        assert_eq!(paths.values, ["a.txt"]);

        let broken = server
            .completion(complete("path", "", &[("rev", "no-such-rev")]))
            .await
            .completion;
        assert!(broken.values.is_empty());
    }
}
