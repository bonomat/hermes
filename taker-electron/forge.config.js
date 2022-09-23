module.exports = {
    "packagerConfig": {
        "name": "ItchySats Desktop",
        "appBundleId": "ItchySats",
        "icon": "images/icon",
        "overwrite": true,
    },
    "makers": [
        {
            "name": "@electron-forge/maker-squirrel",
            "config": {
                "name": "taker_electron",
                "setupIcon": "images/icon.ico",
            },
        },
        {
            "name": "@electron-forge/maker-zip",
            "platforms": [
                "darwin",
            ],
        },
        {
            "name": "@electron-forge/maker-dmg",
        },
    ],
    "publishers": [
        {
            "name": "@electron-forge/publisher-github",
            "config": {
                // todo: change to itchysats/itchysats
                "repository": {
                    "owner": "bonomat",
                    "name": "hermes",
                },
                "icon": "images/icon.icns",
            },
        },
    ],
};
