-- SQLite 3.35+ 支持 DROP COLUMN；回滚本迁移即去掉该列
ALTER TABLE servers DROP COLUMN required_java_major;
