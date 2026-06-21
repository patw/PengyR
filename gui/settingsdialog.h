#pragma once
#include <QDialog>
#include <QJsonObject>
#include <QLineEdit>
#include <QComboBox>
#include <QSpinBox>
#include <QTextEdit>
#include <QPushButton>

class SettingsDialog : public QDialog {
    Q_OBJECT
public:
    explicit SettingsDialog(QJsonObject config, QWidget* parent = nullptr);
    QJsonObject config() const { return m_config; }

private slots:
    void fetchModels();

private:
    QJsonObject m_config;
    QLineEdit* m_baseUrl;
    QLineEdit* m_apiKey;
    QComboBox* m_model;
    QPushButton* m_fetchBtn;
    QLineEdit* m_userAgent;
    QTextEdit* m_systemMsg;
    QComboBox* m_toolConfirm;
    QSpinBox* m_contextKeep;
    QComboBox* m_uiScale;
    QSpinBox* m_toolTimeout;
};
