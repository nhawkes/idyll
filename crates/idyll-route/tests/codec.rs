use idyll_route::Route;

#[derive(Route, Debug, PartialEq)]
enum R {
    #[route("/")]
    Index,
    #[route("/posts/{slug}")]
    Post { slug: String },
    #[route("/docs/{path*}")]
    Doc { path: Vec<String> },
}

#[test]
fn url_is_the_projection() {
    assert_eq!(R::Index.url().as_str(), "/");
    assert_eq!(
        R::Post {
            slug: "hello".into()
        }
        .url()
        .as_str(),
        "/posts/hello"
    );
    assert_eq!(
        R::Doc {
            path: vec!["single_server".into(), "modelling_a_server".into()]
        }
        .url()
        .as_str(),
        "/docs/single_server/modelling_a_server"
    );
    // empty catch-all is just the literal prefix
    assert_eq!(R::Doc { path: vec![] }.url().as_str(), "/docs");
}

#[test]
fn parse_is_the_inverse() {
    assert_eq!(R::parse("/"), Some(R::Index));
    assert_eq!(
        R::parse("/posts/hello"),
        Some(R::Post {
            slug: "hello".into()
        })
    );
    assert_eq!(
        R::parse("/docs/single_server/modelling_a_server"),
        Some(R::Doc {
            path: vec!["single_server".into(), "modelling_a_server".into()]
        })
    );
    // unknown shapes are the typed 404
    assert_eq!(R::parse("/nope/x"), None);
    assert_eq!(R::parse("/posts"), None); // missing the required segment
}

#[test]
fn round_trips_through_encoding() {
    // a slug with a space and a slash: both must survive, and the slash must NOT split the
    // segment (that's why the catch-all is Vec<String>, and a single is one segment).
    for r in [
        R::Post { slug: "a b".into() },
        R::Post { slug: "a/b".into() }, // one segment containing a slash
        R::Doc {
            path: vec!["a/b".into(), "c d".into()],
        },
    ] {
        let url = r.url();
        assert_eq!(R::parse(url.as_str()), Some(r), "round-trip via {url}");
    }
}

#[derive(idyll_route::Segment, Debug, PartialEq)]
enum Slug {
    #[segment("about")]
    About,
    Post(String),
}

#[derive(idyll_route::Segment, Debug, PartialEq)]
enum Format {
    #[segment("json")]
    Json,
    #[segment("html")]
    Html,
}

#[test]
fn a_segment_without_a_default_claims_only_its_literals() {
    use idyll_route::Segment;
    assert_eq!(Format::parse("json"), Some(Format::Json));
    assert_eq!(Format::parse("xml"), None);
    assert_eq!(Format::Html.segment(), "html");
}

#[derive(Route, Debug, PartialEq)]
enum Site {
    #[route("/")]
    Index,
    #[route("/{page}")]
    Page { page: Slug },
    #[route("/by/tag/{tags*}")]
    Tags { tags: Vec<Slug> },
}

#[test]
fn a_segment_type_parses_the_difference() {
    assert_eq!(
        Site::parse("/about"),
        Some(Site::Page { page: Slug::About })
    );
    assert_eq!(
        Site::parse("/a%20b"),
        Some(Site::Page {
            page: Slug::Post("a b".into())
        })
    );
    assert_eq!(
        Site::parse("/by/tag/about/x"),
        Some(Site::Tags {
            tags: vec![Slug::About, Slug::Post("x".into())]
        })
    );
    for site in [
        Site::Index,
        Site::Page { page: Slug::About },
        Site::Page {
            page: Slug::Post("a/b".into()),
        },
        Site::Tags {
            tags: vec![Slug::Post("c d".into()), Slug::About],
        },
    ] {
        let url = site.url();
        assert_eq!(
            Site::parse(url.as_str()),
            Some(site),
            "round-trip via {url}"
        );
    }
}
