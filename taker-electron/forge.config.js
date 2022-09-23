module.exports = {
    "packagerConfig": {
        "name": "ItchySats Desktop",
        "appBundleId": "ItchySats",
        "icon": "logo.icns",
        "overwrite": true,
        "appVersion": "0.6.2",
    },
    "makers": [
        {
            "name": "@electron-forge/maker-squirrel",
            "config": {
                "name": "taker_electron",
            },
        },
        {
            "name": "@electron-forge/maker-zip",
            "platforms": [
                "darwin",
                "linux"
            ],
        },
    ],
    "publishers": [
        {
            "name": "@electron-forge/publisher-github",
            "config": {
                "repository": {
                    "owner": "bonomat",
                    "name": "hermes",
                },
            },
        },
    ],
};
