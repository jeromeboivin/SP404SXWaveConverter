PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share

.PHONY: all install

all:
	cargo build --release --locked

install: all
	install -Dm755 target/release/sp404sx-wave-converter "$(DESTDIR)$(BINDIR)/sp404sx-wave-converter"
	install -Dm644 assets/icon-256.png "$(DESTDIR)$(DATADIR)/icons/hicolor/256x256/apps/sp404sx-wave-converter.png"
	install -d "$(DESTDIR)$(DATADIR)/applications"
	sed 's|@BINDIR@|$(BINDIR)|g' sp404sx-wave-converter.desktop > "$(DESTDIR)$(DATADIR)/applications/sp404sx-wave-converter.desktop"
	chmod 644 "$(DESTDIR)$(DATADIR)/applications/sp404sx-wave-converter.desktop"
