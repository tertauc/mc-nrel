use serde::{Deserialize, Serialize};

/// 下载源。BMCLAPI 是国内常用的 Minecraft 文件镜像。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MirrorKind {
    Official,
    #[default]
    Bmclapi,
}

impl MirrorKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Official => "官方源 (Mojang)",
            Self::Bmclapi => "BMCLAPI 镜像 (推荐)",
        }
    }

    pub fn manifest_url(self) -> &'static str {
        match self {
            Self::Official => "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json",
            Self::Bmclapi => "https://bmclapi2.bangbang93.com/mc/game/version_manifest_v2.json",
        }
    }

    /// 把 Mojang 官方地址重写为镜像地址。
    pub fn rewrite(self, url: &str) -> String {
        if self == Self::Official {
            return url.to_string();
        }

        const RULES: &[(&str, &str)] = &[
            (
                "https://piston-meta.mojang.com",
                "https://bmclapi2.bangbang93.com",
            ),
            (
                "https://launchermeta.mojang.com",
                "https://bmclapi2.bangbang93.com",
            ),
            (
                "https://piston-data.mojang.com",
                "https://bmclapi2.bangbang93.com",
            ),
            (
                "https://launcher.mojang.com",
                "https://bmclapi2.bangbang93.com",
            ),
            (
                "https://libraries.minecraft.net",
                "https://bmclapi2.bangbang93.com/maven",
            ),
            (
                "https://resources.download.minecraft.net",
                "https://bmclapi2.bangbang93.com/assets",
            ),
        ];

        for (from, to) in RULES {
            if let Some(rest) = url.strip_prefix(from) {
                return format!("{to}{rest}");
            }
        }
        url.to_string()
    }
}
