//! The schemas of the two internet tools, `web_search` and `fetch_url`,
//! as rpctl's `SEARCH_TOOL` and `FETCH_TOOL` (chat.py 53-124) offer them.
//! Key order is kept: the chat template renders them with `tojson`. What
//! they do is [`crate::search`] and [`crate::webfetch`], run by viy-agent's
//! chat.

use serde_json::Value;
use serde_json::json;

/// `web_search`'s schema.
pub fn search_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "web_search",
            "description": "Search the live web. Returns titles, URLs and an excerpt of each \
                page. Use this whenever the answer depends on facts you may not know, on \
                anything recent, or when the user asks for links -- your own memory of URLs \
                is unreliable and often produces dead pages. Cite the URLs returned here. To \
                read a result in full, pass its URL to fetch_url.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query, phrased as you would type it into a search engine.",
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "How many results to return (1-10). Defaults to 5.",
                    },
                    "recency": {
                        "type": "string",
                        "enum": ["day", "week", "month", "year"],
                        "description": "Only results from the last day, week, month or year.",
                    },
                    "site": {
                        "type": "string",
                        "description": "Only results from this domain, e.g. docs.python.org.",
                    },
                },
                "required": ["query"],
            },
        },
    })
}

/// `fetch_url`'s schema.
pub fn fetch_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "fetch_url",
            "description": "Read a web page and return its text (Markdown-like; links appear \
                as [n] with their URLs listed after the text). Long pages come in parts: pass \
                start=<the next start it gives> for the next part, or look_for=<words> to get \
                only the passages about them. Use this, not curl or a script, to read web \
                pages. Public http(s) URLs only.",
            "parameters": {
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The http(s) URL to read."},
                    "look_for": {
                        "type": "string",
                        "description": "Words to find in the page; returns only the passages that match.",
                    },
                    "start": {
                        "type": "integer",
                        "description": "Character offset to continue from, as given by the previous part.",
                    },
                },
                "required": ["url"],
            },
        },
    })
}
