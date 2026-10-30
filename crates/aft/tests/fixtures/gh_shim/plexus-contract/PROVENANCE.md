# Provenance of this directory

Copied byte-for-byte from the Plexus repository (`cortexkit/plexus`),
commit `0025bdf6cdca8c2d9c6aa10409df9424260d7b5e` (2026-10-28,
"mason: refuse foreign-PR review-thread resolves"), directory
`crates/plexus-core/tests/fixtures/gh_shim_wire/`:
`README.md`, `requests-contract/` (144 vectors) and `replies-contract/`
(17 vectors). This replaces the contract copy from commit `377e1874`.
As in the previous vendoring, Plexus's AFT-derived `requests/` captures and
its `PROVENANCE.md` are not copied; this file records AFT's contract import.

Those files are Plexus's own: each vector is proven on every Plexus test run
against the real `gh.route` facade (see the "Request and reply contract
vectors" section of the vendored `README.md`). AFT pins its gh shim tests
to them: the shim's pre-signing round trip once passed against a stand-in
that accepted the `gh_route_schema` envelope field live Plexus refused, so
the stand-in must not be more lenient than Plexus's real route. Re-copy from
a newer Plexus commit to pick up contract changes, and update this file's
commit, date, counts and hashes together.

## SHA-256 of copied files

The hashes below cover the exact Git blob bytes at the pinned Plexus commit;
`PROVENANCE.md` is maintained by AFT and is not part of the copied set.

| Path | SHA-256 |
| --- | --- |
| `README.md` | `c07a1d453c58a00d6e067ab70a12059ed815c4c2b714ef6ec63f4da10618204f` |
| `replies-contract/pr-draft-not-own.json` | `2291c88046bf5f97179f06a26091861beda31b3fc66cab8c1d88439ca8c9a3e5` |
| `replies-contract/pr-edit-not-own.json` | `fdb61597f7b15e9e1da6ca5648250e1fcae09a98ae42d9ea3552111695c71621` |
| `replies-contract/pr-edit-pull-request-not-found.json` | `c391b6fe818cb1fa6318ad65e44c540c1153ab82c5ec71bd67680b91a2f3ed90` |
| `replies-contract/pr-ready-not-own.json` | `1cc3bcdc280efb26f96ee91275c99df7085e3b25a180b0f3c6789cdf8718b34c` |
| `replies-contract/pr-ready-pull-request-read-incomplete.json` | `3f61bb24e26b692790b51f82b025c9d0818ce280d0f3b12c7636552e162dac54` |
| `replies-contract/resolve-thread-graphql-errors.json` | `332ebe6a0e7ba8e0c9708b360b5817dca2f0cf4f8e28bd8d169965455f64467d` |
| `replies-contract/resolve-thread-not-own.json` | `96f53998f58735d57549ec7fbcdcf73f030eab3797da2d26e4751b506d08394e` |
| `replies-contract/resolve-thread-not-present.json` | `d40d1bb65a9e57046416b4766676b743b831e5068af1e2997cfe96c1abaa1ec3` |
| `replies-contract/resolve-thread-own.json` | `c34059de27a62ccf44224775d2552cd0aa611f600f3e5fe57a0be3891f11496b` |
| `replies-contract/resolve-thread-repository-mismatch.json` | `63783e12c3d58fce91439133a9c160150f409ed66c9a698f89e67e38138adb61` |
| `replies-contract/stack-add-pr-not-own.json` | `c26710f9132c22d90ffc243a6bf973c8304755d40b5bfe1dec1f59a4457d6e4c` |
| `replies-contract/stack-create-pr-not-own.json` | `947af954b5223b87780ae87299aaad0b9e9f14173138dbb86eea63fddc9a9405` |
| `replies-contract/stack-unstack-foreign-member.json` | `1a49d1c3aeb8ca28c32ed14b0d513c5e516e0a0fa9233892ef7ea3a10956101a` |
| `replies-contract/stack-unstack-foreign-stack.json` | `90a1f6c7d98266173d6c0706562ec4da69de37e407bcc40d648e673e830e867b` |
| `replies-contract/stack-unstack-pr-not-own.json` | `3ac6c70c153772efeeec861640ad85689e84176b08a037806b8595fc5ff502d5` |
| `replies-contract/stack-unstack-stack-read-incomplete.json` | `d9908a4a288aad6c9c2e7fc342aa07d41bffd4a659342f2b624320bbeeb49e8c` |
| `replies-contract/unresolve-thread-repository-mismatch.json` | `fa00db99df7a386d17e23ffabe5cc7f563137c51b3adcdd0b02fc77028578e46` |
| `requests-contract/accepted-close-issue.json` | `84724715a3bcbad824af0fb9efd60216cedef4986b6b89f8020b5e51c14fba62` |
| `requests-contract/accepted-close-pr.json` | `b21a42f6b45f435ef533f39b23634dbb80791c53224a4d6050762ddc7b3ea95a` |
| `requests-contract/accepted-comment-edit-last-issue.json` | `e21be35c07cfe8410aff80ccd68e68701c09e58ae4360821dff76c302190fc43` |
| `requests-contract/accepted-comment-edit-last-pr.json` | `3a598d6bed5ab89b4cba0c86e2eab783131c915d068824fc04a67bae998b3a4c` |
| `requests-contract/accepted-comment-edit.json` | `097a48f3c001e34bfd83deeacab8f3e13e013689887ee8ec5a44e4d57fe8dde0` |
| `requests-contract/accepted-comment-issue.json` | `b58fba9867bf768c981908ab8a61aae8b7ddf0fa68db937133680b9304bd4082` |
| `requests-contract/accepted-comment-pr.json` | `9fcb579bde8eb522baae44318e53b30762f61ff17f11c9ad99f6949afc0bd0a5` |
| `requests-contract/accepted-issue-create.json` | `baad10d324256af31f441812d24bd8b10f6999850b527cf13ba97aaab4b898ea` |
| `requests-contract/accepted-issue-edit-labels.json` | `11d9b9d282da18e47d255252df58afc417e14b2cabd065b6a0adc33e887cdf66` |
| `requests-contract/accepted-issue-edit-title-body.json` | `4e0b611dc1a8dd0ca52a3d454cf305710d1b14154ff812afb5ca701930d611c8` |
| `requests-contract/accepted-pr-create.json` | `f360d1a044df48012858fbe1d47b0f89d444a3822c87efa9c549ce272bc81e91` |
| `requests-contract/accepted-pr-draft.json` | `7bd206ba654a06d6d7b5ba0cf481d0d1629cd1ecf918ba80c2296428d706157b` |
| `requests-contract/accepted-pr-edit.json` | `f932f7ccc9c7c516e2b451319e3c6d7198f5adfe1542d6e5dfd7d8eff1895fa9` |
| `requests-contract/accepted-pr-ready.json` | `94993c4e82c2ce6f573cd5f2e23509bf73928cd078ef2cf5a6c5dbe2fc222e8b` |
| `requests-contract/accepted-pr-review.json` | `a7dc3ba0bd5ff5920de26b7bc89f8b6504331879f64c8dfe508e1d8e656affbb` |
| `requests-contract/accepted-reaction-issue.json` | `5357630f73147e9d1b26a8b074e254eeecb1cf2c5999122529e41e6724f11a78` |
| `requests-contract/accepted-reaction-pr.json` | `e69a2dbc253c55a8447ac1163064ec43a643a27d98560e2c548ba0cca406d70a` |
| `requests-contract/accepted-reopen-issue.json` | `d2f29955011326f92d46a2213078032e72405d231ab29c0e1a0eeab8e7d40812` |
| `requests-contract/accepted-reopen-pr.json` | `385b528139a80ac0e3e0c80a004e80bcfda6948343b7efcf61ef94bd4d44f742` |
| `requests-contract/accepted-resolve-thread.json` | `6914e9e5ade17d7f606e7ab75c4db3a0a795e2fe83671b1d017cda7a35bb8217` |
| `requests-contract/accepted-review-reply.json` | `e5e3c74ad65de918ec6d5397ef8334c8831e006ee52845330ac3876d0cc80008` |
| `requests-contract/accepted-stack-add.json` | `3b9145935df4ee38af8d3745fe18a36b602d62bba9a512be6e31866b9d8690d8` |
| `requests-contract/accepted-stack-create.json` | `c777ceca419fc5911a5586c455b6e7b4706327c410f1a54f25da99557fb34e0d` |
| `requests-contract/accepted-stack-unstack.json` | `6ca11bed24ad893ddf0cbfa92bbdbbae9ddba27b65bc0cd3b0c8d31eb0e6b43b` |
| `requests-contract/accepted-unresolve-thread.json` | `0daffc3b1bcd55c11ebefbccfc677ad2ccc1a5172c46f2411149f583a248db3e` |
| `requests-contract/refused-any-kind-full-name.json` | `e4b114d38184364872f917cdd020510f93c4f2aefa89efc447d8bca9a3318bfc` |
| `requests-contract/refused-any-kind-owner.json` | `088aaf0eb5e0bf8fcce390168b17fcd4810868fd2d4a1a30db13ac94c55b5a71` |
| `requests-contract/refused-any-kind-path-repository.json` | `969f3b6fa4e6e173eef43dac570fc37fe2e6945e1fd35f74c9c648fd9786350e` |
| `requests-contract/refused-any-kind-repo.json` | `8e39578f5c2100ce570a3c5d01528e96c023022c0b5eceae7251b66534cbab59` |
| `requests-contract/refused-any-kind-repositories.json` | `16dfcfd9322adc38e7bebf82fb74c9807db1eadb8ef61ba1fc279e16ff25be2b` |
| `requests-contract/refused-any-kind-target-repository.json` | `51187f4f89e86f4c5c2ef893688a2bf66243e3c1611174f3a2567bec96be802d` |
| `requests-contract/refused-close-body-field.json` | `fd1d9bb0d3a46376f9363d07e4299806859f835479a6fa587336a60152714fd7` |
| `requests-contract/refused-close-comment-wrong-type.json` | `5cd228075ac718cf3ad81a0a9545cdf837454870264929300c336f970cbd5000` |
| `requests-contract/refused-close-title-field.json` | `a703f191f33c8a7e2806af4aa42ebda1695e5b49a91fe7e9e8194e5f46ca50bd` |
| `requests-contract/refused-comment-body-issue-number.json` | `5f234ba12e51705da26aa49ea6bc51a9b2bf65876b8e0417330a9a194fa35024` |
| `requests-contract/refused-comment-body-wrong-type.json` | `61f2c8412569bfa7f0dd602eefba1e978de8aaaae220fc39332e1327d90dbbe9` |
| `requests-contract/refused-comment-edit-comment-id-not-numeric.json` | `67367030f39d009525d5a4d75727ac31178046b1dc334b41315be26c52f939e2` |
| `requests-contract/refused-comment-number-not-numeric.json` | `c64b1b7f8f6e9dfb11374a20309f869afb50d332cc6213e70aa37d92c6039db8` |
| `requests-contract/refused-issue-create-body-wrong-type.json` | `8513194a53f9399dabac53db641dc52b204cc38310290a2c77dfd0d728cc701d` |
| `requests-contract/refused-issue-edit-add-labels-not-array.json` | `568945433b7bec35417937943ac41037887996089d7e6c364c3a0cf4886247c4` |
| `requests-contract/refused-issue-edit-labels-non-string.json` | `269cc2aac5b64eb51c42987c199007e2c7be29b46fff22293bf0e47b44a2d610` |
| `requests-contract/refused-issue-edit-milestone.json` | `934719cfed4055df1d20798a15cac8b71b9e4e6d683f654c559e5283fc165460` |
| `requests-contract/refused-issue-edit-state.json` | `7facb615f6af0e270641836548cb0c50d6b4f2c387443f59c2572fdc2c3e7f3d` |
| `requests-contract/refused-pr-create-body-wrong-type.json` | `ac6e5c150a41f30edf7702ccc27911cb47b5a1f6c6bc6b4995b52070e36d62e4` |
| `requests-contract/refused-pr-create-empty-head.json` | `862660e0a5aff1f29d21b5e0ccc06ec1b9503beea4745e13b4b75b67dac7b2c1` |
| `requests-contract/refused-pr-create-head-cross-repository.json` | `8f6c8d33b0b64e9415513e2e12e953ffb45c6e1ea9459aa3bd88536765ade409` |
| `requests-contract/refused-pr-create-head-equals-base.json` | `89df772335a3f141713bb58f131c0276249199c98b6b75f033104789bcd16375` |
| `requests-contract/refused-pr-create-head-wrong-type.json` | `b437a9d421ffbb0ae3db3fdf07a964e2e9fb7878dbc4ade4249640c1afd24cd8` |
| `requests-contract/refused-pr-create-missing-base.json` | `1893534b58bacf332c49857e675ab87c39662170396fb47b4209bac37b868528` |
| `requests-contract/refused-pr-create-missing-body.json` | `80a90ce03d9f225f5963c8a85fc038d2ef5f1e9a5d96589b922b7a67215c7424` |
| `requests-contract/refused-pr-create-missing-head.json` | `500d0f3e69171aae761ced462b36338d9a3718efd2fdedd164a87c7c5f3ee0a2` |
| `requests-contract/refused-pr-create-top-level-base.json` | `fbc996ef654ac962a4a2d8d2d6bbaee2f1b34c04b2815a1288575e6e9088c5da` |
| `requests-contract/refused-pr-create-top-level-draft.json` | `5c13a48675ca3ab2afd9b90b90302e0d11e1a7df4b47f60f1088f0989c046978` |
| `requests-contract/refused-pr-create-top-level-head.json` | `6ee89f7e0583ea0184c3b586a4f01044df0c83ff5335ddc7489dcdbf2e014f2d` |
| `requests-contract/refused-pr-create-top-level-title.json` | `181813cbecc989e3abc43577310b891c21463f347de3ca2f58a89a6432a64ca3` |
| `requests-contract/refused-pr-draft-body-field.json` | `28cc1083383a1d50163e557f963ba5719702387cce47f963cbf585aa7ecc31a7` |
| `requests-contract/refused-pr-draft-missing-number.json` | `f98ef8ea1b03b6447a0e1c14abb744206f7d8b96257377a4d7ccf16bdcb11396` |
| `requests-contract/refused-pr-draft-missing-undo.json` | `16483a91f0eba886ae92d97a0f699bd2737f35106c9915bb99860e7161145da5` |
| `requests-contract/refused-pr-draft-number-not-numeric.json` | `cabb61f2b238ffea18eb0fb1b0115860793048ee86bc059b389a3884000e8296` |
| `requests-contract/refused-pr-draft-target-field.json` | `89c36bdfde2dd5f0870b59fdd092e509983567908fb227af4fa91ca33fdc10a3` |
| `requests-contract/refused-pr-draft-undo-wrong-type.json` | `414bd85b442671fc376447160ab45d4c489373090453aad3083a0729aa0b7487` |
| `requests-contract/refused-pr-draft-unknown-top-level-field.json` | `45924231b538349090a7ecef79fe903ec213dc4acc2e174e3be1098db4e3476a` |
| `requests-contract/refused-pr-edit-author-scope-not-own.json` | `8148389b388cdd642ef1b51f4f3d5224fb8c2fd1362768cc4ee8aa8850e575cd` |
| `requests-contract/refused-pr-edit-body-base.json` | `1c2c163c84f7ea091b2265e181b7eb68af3159086d587cec6c2f81f77d516282` |
| `requests-contract/refused-pr-edit-body-wrong-type.json` | `40b52419a381b999d84146017c4261e66f427f4442d208eb2ac863181ec73fe6` |
| `requests-contract/refused-pr-edit-empty-body.json` | `62c6bf30dd323ebae88bb604eb30a6f83511b7951479fbde7bcdf44badea59c5` |
| `requests-contract/refused-pr-edit-missing-author-scope.json` | `6b3678c0dbc352f57b3b510b97743da87936a0eb607e63bdeae19d0e4e3f7e92` |
| `requests-contract/refused-pr-edit-missing-body.json` | `d7edb7e48efc880f0da6f93cfd2e33de0030050834fb1c00c62e138ae09308d6` |
| `requests-contract/refused-pr-edit-missing-number.json` | `c8fd42317181b9059b620bedebe08061b25c283529ee95dcea35424c6bce4734` |
| `requests-contract/refused-pr-edit-missing-target.json` | `65e1b6d581fe5bbf328da562b1a601415fa3d3d4607bd9f983ce68dccc30c144` |
| `requests-contract/refused-pr-edit-number-not-numeric.json` | `18805c92418911a5a5878319429dda3fa69f70454f226f9cb2631eace8cf74a2` |
| `requests-contract/refused-pr-edit-number-zero.json` | `cbe88c7cac6bb30eb7030af9f573aa82cdde30accb052bf3ced1c80f9ca7be3c` |
| `requests-contract/refused-pr-edit-owner.json` | `45aff49f7b03ac322b8bc40d84ebea801153a18ae9441c1c989f5a0499c29909` |
| `requests-contract/refused-pr-edit-target-wrong-type.json` | `8ee442d756ae08786512155fd37980325b4b031b78a3ceb908d14f0b535b8552` |
| `requests-contract/refused-pr-edit-title-wrong-type.json` | `549ba8599a2e1794b10c02289960ad299853e5cd3d6cc8759661c4b29902ba8b` |
| `requests-contract/refused-pr-edit-unknown-body-field.json` | `a17d3993a5a3e4966c5c22de225763d803e8fe49f6b164e4acea25d7a29da0b5` |
| `requests-contract/refused-pr-edit-unknown-target-field.json` | `125c812ce9c27a145a87c2e7990389948d1b4bc37555e069b5fb51855e1ee1ee` |
| `requests-contract/refused-pr-edit-unknown-top-level-field.json` | `e166e6c91cb51661d7a3bd94ee982307980fad382f2b3b8cb359c978c29d7536` |
| `requests-contract/refused-pr-ready-author-scope.json` | `081a93301d29348495c5af6dc94f411463cb9cc3333c330666eb0b9b5a48ba7e` |
| `requests-contract/refused-pr-ready-body-field.json` | `e893c5ce866c552a04d5129c015462330e0b1bd7e3117d75ae310d1b2c80644a` |
| `requests-contract/refused-pr-ready-missing-number.json` | `94f766df03494cd5f24b6a98de3d8715585a466f1fcedbfa8ff76784fb7408cd` |
| `requests-contract/refused-pr-ready-missing-undo.json` | `21f1b81a06516e54e156f70415fbf9d4f307b724097dbd44f3efe9d2e13ebd7f` |
| `requests-contract/refused-pr-ready-number-not-numeric.json` | `2c2f1675ee691bf2dd9d379bff7ab1c2f781920be5bed7c27438ec1562ec0471` |
| `requests-contract/refused-pr-ready-repositories.json` | `edf22ba68a0363dcca374f3f2feae712a466f78760a4696db3f29a119fa8b0f8` |
| `requests-contract/refused-pr-ready-target-field.json` | `b0ce3e3cf1bbac52324d0a4e0d28b5fd1e81254df78aa8bcbd50a8eccd1b00c7` |
| `requests-contract/refused-pr-ready-undo-wrong-type.json` | `e4cfec07ba965e3b258f0e6d2d279da30ca1280f337266439c3d1d71f672b6b8` |
| `requests-contract/refused-pr-ready-unknown-top-level-field.json` | `4503d89b1ca08d0561332904d5c3fcdb71c7ebe3630700117c6a05b90efa23bd` |
| `requests-contract/refused-pr-review-event-pending.json` | `0395fd182412f928cef84ef8d5aaf321785ee97c47342de4f2f05c7f34a359f1` |
| `requests-contract/refused-pr-review-missing-event.json` | `14d791481ed65f661ba3e624f67841102c52f01342f1f0c827e93b195d04aea5` |
| `requests-contract/refused-reaction-missing.json` | `822cd245b177f6e9757e1591bba978a3b1cec5e35f9e3fbb1daeb4d4421b98d8` |
| `requests-contract/refused-reaction-not-in-vocabulary.json` | `f00a640b468ce7bd3007aa121c3a6fe0f210f226e914516364e899ef03bbdf24` |
| `requests-contract/refused-reopen-body-field.json` | `a59b730625a0e5b85d9549c3abe6697d944d0e77f37d6172208a74fb46a3441c` |
| `requests-contract/refused-reopen-comment-wrong-type.json` | `14eff41f464c96977acfec9b178fef81c9e288484eb0d1c71fc53c1202b4a621` |
| `requests-contract/refused-reopen-title-field.json` | `57b032a7912ea0a75af4a5d8cba3945d78224cc40cd76673eceb8bfdb0250cc7` |
| `requests-contract/refused-resolve-thread-missing-body.json` | `d89d975655470af279aac83b712f39b3606e67bc6e76caae45921f050ca4e694` |
| `requests-contract/refused-resolve-thread-missing-thread-id.json` | `f5f9c9f4974f9c0ca288bc24867f1a16c7ed44ed3e590960cacdea52a151ab52` |
| `requests-contract/refused-resolve-thread-thread-id-empty.json` | `b6557c77db6b29c8e44af08271f04a90fd602e4f3e34ebf4f2a670fe9f48eec0` |
| `requests-contract/refused-resolve-thread-thread-id-wrong-type.json` | `e2cf0b2c63205304d967cc1568fba8b37403b2d935f48fd48fa5c13361ecbb6d` |
| `requests-contract/refused-resolve-thread-unknown-body-field.json` | `f57c6d83b3415340740c5f0ea8503143634d968107c8f54fdaf6b544cc2a5c97` |
| `requests-contract/refused-resolve-thread-unknown-target-field.json` | `db6030458a786f29700fdb564e7ed059fc4924e46919640a76742a8099f58154` |
| `requests-contract/refused-resolve-thread-unknown-top-level-field.json` | `58b173143f1d80b099177f7a6ed44a29f22cc1899ba6fec789e3de1a3256c3e4` |
| `requests-contract/refused-review-reply-author-scope.json` | `500d650edcfebd77813ff8e79d0e7b06e406392765296b816c6bc04e78aa8e9e` |
| `requests-contract/refused-review-reply-body-text-wrong-type.json` | `e7e9f4901becc4c11218b7ca65e98f233b82a60476d6afba603f41a023027cad` |
| `requests-contract/refused-review-reply-comment-id-not-numeric.json` | `e3fb121cfc6502850fa7fb555d7bd345ef55c4e3696fbc3bcc156ed7a027fd13` |
| `requests-contract/refused-review-reply-missing-body-text.json` | `3848807c4597cb68b020c03b30ed80bd7e94151ebe0c77761910da09ba78fa71` |
| `requests-contract/refused-review-reply-missing-comment-id.json` | `dd5ceeba5b2fe3f6616b0e6aa196f8bed9c997e3a9dccb7fc0da73d264770425` |
| `requests-contract/refused-review-reply-missing-number.json` | `b9eeb40f5db01c9b945c9295754f131bc7b3de9342ee7d3eebe610bd9f149621` |
| `requests-contract/refused-review-reply-unknown-body-field.json` | `98ff89b2c558758fce5a72b73466a9c9f0e3b17267fa886bd5a01d996f5b6ffd` |
| `requests-contract/refused-review-reply-unknown-target-field.json` | `4d0b5a1e9a296d6b9e4c94ca203c0d6aa16c93b0d3972fc21d07df6dd886c3df` |
| `requests-contract/refused-review-reply-unknown-top-level-field.json` | `9bbc5778a19a26acfadea745a79c68ba9f0f78c7e821faafd46da9a9a42f9eb1` |
| `requests-contract/refused-stack-add-missing-pull-requests.json` | `2eaec0a05dc35c54927e0a02ba2460c06a48a826f97d88e3f093f4d6da203d2c` |
| `requests-contract/refused-stack-add-missing-stack-number.json` | `878c2687376ec1d1d767f01dba80ba73bb9a678dd5549262653dfb0bec7966d1` |
| `requests-contract/refused-stack-add-pull-requests-duplicate.json` | `0dd176a429ac31905373fabd8ecbd7e3bddce958b643c7d9bbf449f25d972335` |
| `requests-contract/refused-stack-add-stack-number-wrong-type.json` | `04b9584312f4249d40505ab09f6e1498f4e522d9f0d2a313e8b507c30ff155a9` |
| `requests-contract/refused-stack-add-unknown-body-field.json` | `a772ea085825a517cfcdc2b6799f83fb93e7147285a06539d461d15dfb7242f2` |
| `requests-contract/refused-stack-add-unknown-target-field.json` | `e51e67cb664f1d97755a8e27c92f567877463b3d02c7b43a6745f0833e6785db` |
| `requests-contract/refused-stack-add-unknown-top-level-field.json` | `041849517b781fda765fbddabb212a6591f09cadebf0675cda8f72a6a92449b7` |
| `requests-contract/refused-stack-create-missing-body.json` | `6ba4dd532079ea34610173140e29ce55e56a5bd0f13a84125665af72fc3dd18e` |
| `requests-contract/refused-stack-create-missing-pull-requests.json` | `ccb0dc1f02f8fa1338dd8efba5143a1f70f0778f1f5bd74f36137eb1c36f8499` |
| `requests-contract/refused-stack-create-pull-requests-duplicate.json` | `b7e6bbba198961da4bb664bd2f944414e199b2bfcbee34d0220a73a7bdad7109` |
| `requests-contract/refused-stack-create-pull-requests-empty.json` | `bba6611dbac771bebb107883d5e7d726b318a85da3382aaec42e918be722ed25` |
| `requests-contract/refused-stack-create-pull-requests-negative.json` | `fd520bf356f2154ab2c86fdd105e14345ab524dde34cbfee7c4d02165e3b5333` |
| `requests-contract/refused-stack-create-pull-requests-over-fifty.json` | `1e70b45563b460d485e9351e4b43ac04bebb479733d220bbad1ce5da60b5f815` |
| `requests-contract/refused-stack-create-pull-requests-string-item.json` | `867637ecba3df5f8198a1bd96d960beae52508af7c251ca01c9719f01a8f0eb8` |
| `requests-contract/refused-stack-create-pull-requests-wrong-type.json` | `a21a4295c62725df244d18678bf53b582cee17122ae3931b9098bd90cb069ce1` |
| `requests-contract/refused-stack-create-pull-requests-zero.json` | `f8d6a805fb03816aa8feffe1fcb75217ddc65671459a6913af9d53d31c263c66` |
| `requests-contract/refused-stack-create-unknown-body-field.json` | `ac595fccb4fd14c158da65abcd20fbf8346f9339e256ee873adfa516f4d52d4f` |
| `requests-contract/refused-stack-create-unknown-target-field.json` | `c9a534b937d4f2bbbb4434fbcf1ae5e311544bd33f60f175ee2e479e6ac9184d` |
| `requests-contract/refused-stack-create-unknown-top-level-field.json` | `4626225f72c121044cef119dd4799d72de36549ced362a3641f1118b3f9a8457` |
| `requests-contract/refused-stack-unstack-missing-body.json` | `f33705dfc8ac8d97343629a0633c90c47c1dd89ed4295d7e3c7f3ea6ac979e18` |
| `requests-contract/refused-stack-unstack-missing-stack-number.json` | `68fb61e85742eab2dd31857a4bf744ad5c1fd9be498fcb41e6441d05d55b934a` |
| `requests-contract/refused-stack-unstack-stack-number-zero.json` | `b98b9f70b5845c380fcb5bdd1f8751037e8242a72fd6fb28ff289e50c1a6ad99` |
| `requests-contract/refused-stack-unstack-unknown-body-field.json` | `f34a13d25d6dc0f00100b8ee91c0860dee3249d41b46b93f4c20a39533b761bc` |
| `requests-contract/refused-stack-unstack-unknown-target-field.json` | `e3f08c4aefab665db9024854f8ad193df7a4735f0df9c6748447ecc2d2703606` |
| `requests-contract/refused-stack-unstack-unknown-top-level-field.json` | `af4ba313f81baea349049788bbdb46252cfa562937f7a19581e98f616198be15` |
| `requests-contract/refused-unmapped-pr-merge.json` | `9afc6359c61df912cd38933f9ff18b7c8571b8bd35370b0f9ba88f0626589bd9` |
| `requests-contract/refused-unmapped-verb-pr-merge.json` | `a95f6aec1f164fc4994f4292b7a50f202023c206bf9296319eb3ba32697ec540` |
| `requests-contract/refused-unresolve-thread-missing-body.json` | `a2ea46fc56074d7cf14412ee689b4d18f6c0e686795d959aee32af7eb15cdaef` |
| `requests-contract/refused-unresolve-thread-missing-thread-id.json` | `6b9be5581732e61000b261c6b9c8376dc104100684ac57f6d8b5038dc9bd90a9` |
| `requests-contract/refused-unresolve-thread-thread-id-empty.json` | `1b94e487fcf1a9a119f9b31af486252c68667c6436b2924b085ec6aca7a81eb4` |
| `requests-contract/refused-unresolve-thread-thread-id-wrong-type.json` | `bf51a3a8888e6f047a36b261f522b4859cd331a4cd1cdd1b926d45c5b970a0b1` |
| `requests-contract/refused-unresolve-thread-unknown-body-field.json` | `0604de48edf785588cdbb7e2fe7fa0280b466caa57a254b57aa63c806e3b79f2` |
| `requests-contract/refused-unresolve-thread-unknown-target-field.json` | `0a6e167c576f5bb4434e075bd3592d34ffdb606efa72fdbc2f3bb50377aec6c3` |
| `requests-contract/refused-unresolve-thread-unknown-top-level-field.json` | `617894091d0242f14af90988373dc58b96c142481677e241bad41d8af01e931b` |
