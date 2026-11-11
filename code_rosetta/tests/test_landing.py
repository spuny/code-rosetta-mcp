"""GET / used to be a bare 404. A human opening the URL should learn what it is and how to use it."""

from starlette.testclient import TestClient

from code_rosetta.main import mcp


def _get(path="/", **headers):
    return TestClient(mcp.http_app()).get(path, headers=headers)


def test_root_explains_what_this_is_and_how_to_connect():
    r = _get()
    assert r.status_code == 200
    assert r.headers["content-type"].startswith("text/html")
    html = r.text
    assert "Code Rosetta" in html
    assert "http://testserver/mcp" in html
    assert "claude mcp add --transport http rosetta http://testserver/mcp" in html
    for tool in ("grep_code_tool", "search_nodes_tool", "query_graph_tool"):
        assert tool in html
    assert "no web search UI" in html and "Sourcebot" in html


def test_root_uses_the_public_scheme_behind_a_tls_proxy():
    html = _get(**{"x-forwarded-proto": "https", "host": "rosetta.example.com"}).text
    assert "claude mcp add --transport http rosetta https://rosetta.example.com/mcp" in html


def test_page_has_no_scripts_and_no_external_assets():
    html = _get().text.lower()
    for bad in ("<script", "<link", "<img", "src=", "@import", "url("):
        assert bad not in html


def test_untrusted_host_header_is_escaped():
    html = _get(**{"host": 'x"><b>y'}).text
    assert "<b>y" not in html


def test_healthz_and_mcp_route_still_work():
    with TestClient(mcp.http_app()) as c:  # lifespan running, as in production
        assert c.get("/healthz").text == "ok"
        assert c.get("/mcp").status_code in (400, 405, 406)  # the MCP endpoint, not the landing page
