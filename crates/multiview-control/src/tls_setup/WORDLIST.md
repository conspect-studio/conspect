# Setup-code word list

`bip39-english.txt` is the BIP-39 English word list, copied byte for byte from
<https://github.com/bitcoin/bips/blob/master/bip-0039/english.txt>
(2048 lines, SHA-256 `2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda`,
fetched 2026-11-12). BIP-39 is published under the MIT license (its header:
`License: MIT`); authors Marek Palatinus, Pavol Rusnak, Aaron Voisine, Sean Bowe.

It is used only to render one-time setup codes (8 words = 88 bits) for the
front-door certificate setup page (ADR-0107 section 5a). It is not a key
derivation and has nothing to do with BIP-39 seeds.

`THIRD-PARTY-NOTICES.md` is legal content and is not edited by engineering
work; the attribution entry for this file is raised for the maintainers.
