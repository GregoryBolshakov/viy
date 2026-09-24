use pretty_assertions::assert_eq;

use super::*;

fn texts(input: &str) -> Vec<String> {
    completions(input).into_iter().map(|c| c.text).collect()
}

#[test]
fn names_complete_from_their_start() {
    assert_eq!(texts("/re"), ["/rename ", "/rewind ", "/resume "]);
    assert_eq!(texts("/sta"), ["/stats"]);
    assert_eq!(texts("/stats"), Vec::<String>::new(), "typed out");
    assert_eq!(texts("hello"), Vec::<String>::new());
    assert_eq!(completions("/").len(), COMMANDS.len());
}

#[test]
fn values_complete_after_the_command() {
    assert_eq!(texts("/mode "), ["/mode normal", "/mode auto-yes"]);
    assert_eq!(texts("/effort m"), ["/effort medium"]);
    assert_eq!(texts("/think o"), ["/think on", "/think off"]);
    assert_eq!(texts("/rename x"), Vec::<String>::new(), "free text");
    assert_eq!(texts("/mode auto-yes"), Vec::<String>::new());
    assert_eq!(texts("/compact full x"), Vec::<String>::new());
    assert_eq!(texts("/mo\nde"), Vec::<String>::new());
}

#[test]
fn the_popup_shows_arguments_and_help() {
    let offer = &completions("/mod")[0];
    assert_eq!(offer.label, "/mode [normal|auto-yes]");
    assert_eq!(offer.about, "who answers the approver's questions");
}
